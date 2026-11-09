//! Wiring `[[schedule]]` entries to a Lambda function through `EventBridge`, in process.
//!
//! `cargo lambda deploy` uploads a function; it does not give it a schedule. Every `[[schedule]]`
//! becomes an `EventBridge` rule named `{function}-{schedule}`, targeting the function, with a
//! `lambda:AddPermission` that lets `events.amazonaws.com` invoke it. Unlike the SQS wiring
//! warning, everything the rules need is in the manifest — name and cron both — so the deploy
//! creates them rather than printing instructions.
//!
//! The event the function receives carries the portable cron in `detail.cron`, stamped by an
//! `InputTransformer`: `EventBridge`'s native event names only the rule, and the handler reads
//! `ScheduledTick::cron` for the expression it was declared with. `time` and `resources` are
//! carried through so the tick's `scheduled_time_ms` is the time the schedule was due, and
//! `resources` keeps the rule's own ARN for anyone reading the event raw.

use crate::{output, providers::Task, runtime::block_on};
use anyhow::{Context, Result};
use aws_sdk_eventbridge::types::{InputTransformer, RuleState, Target};
use skyzen_manifest::ScheduleEntry;

/// The Lambda resource-based permission a rule needs to invoke the function.
const INVOKE_ACTION: &str = "lambda:InvokeFunction";

/// The `EventBridge` service principal.
const EVENTS_PRINCIPAL: &str = "events.amazonaws.com";

/// The rules that point `[[schedule]]` entries at a deployed function.
#[derive(Debug)]
pub struct EventBridgeSchedules {
    /// The function to target, as `deploy` names it.
    function: String,
    /// The `[[schedule]]` entries to wire, each becoming one rule.
    schedules: Vec<ScheduleEntry>,
}

impl EventBridgeSchedules {
    /// The step that wires `schedules` to `function`.
    pub const fn new(function: String, schedules: Vec<ScheduleEntry>) -> Self {
        Self {
            function,
            schedules,
        }
    }

    /// The rule `name` becomes when pointed at this function.
    fn rule_name(&self, name: &str) -> String {
        format!("{}-{name}", self.function)
    }
}

impl Task for EventBridgeSchedules {
    fn describe(&self) -> String {
        let names = self
            .schedules
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "wire {} EventBridge schedule(s) to Lambda function `{}`: {names}",
            self.schedules.len(),
            self.function,
        )
    }

    fn run(&self) -> Result<()> {
        block_on(async {
            let shared = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let lambda = aws_sdk_lambda::Client::new(&shared);
            let events = aws_sdk_eventbridge::Client::new(&shared);

            // The deploy that just ran leaves an update in progress, and reading the
            // configuration settles it before the ARN is taken — the same wait the
            // environment-delivery step performs.
            let arn = lambda
                .get_function_configuration()
                .function_name(&self.function)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "could not read Lambda function `{}` to point EventBridge rules at it",
                        self.function
                    )
                })?
                .function_arn()
                .with_context(|| {
                    format!(
                        "Lambda function `{}` reported no ARN to point EventBridge rules at",
                        self.function
                    )
                })?
                .to_owned();

            // arn:aws:lambda:{region}:{account}:function:{name} — the rule's ARN lives in the
            // same partition, region and account, under the `events` service.
            let (region, account) = arn
                .split(':')
                .nth(3)
                .zip(arn.split(':').nth(4))
                .with_context(|| {
                    format!(
                        "`{arn}` is not a Lambda function ARN EventBridge rules can be built from"
                    )
                })?;

            for entry in &self.schedules {
                self.wire_one(&events, &lambda, &arn, (region, account), entry)
                    .await?;
            }
            self.retire_stale(&events, &lambda).await?;
            Ok(())
        })?
    }
}

impl EventBridgeSchedules {
    /// Remove rules — and their invoke permissions — that a previous deploy created and the
    /// manifest no longer declares.
    ///
    /// Skyzen's rules are exactly the ones named `{function}-*`, so the manifest's own set is
    /// the diff: a rule in the prefix but not in the manifest belonged to a `[[schedule]]`
    /// that was renamed or removed, and leaving it would keep firing a schedule the application
    /// no longer has.
    async fn retire_stale(
        &self,
        events: &aws_sdk_eventbridge::Client,
        lambda: &aws_sdk_lambda::Client,
    ) -> Result<()> {
        let prefix = format!("{}-", self.function);
        let declared: std::collections::BTreeSet<String> = self
            .schedules
            .iter()
            .map(|entry| self.rule_name(entry.name.as_str()))
            .collect();

        let mut request = events.list_rules().name_prefix(&prefix);
        loop {
            let listed = request
                .send()
                .await
                .with_context(|| format!("failed to list EventBridge rules under `{prefix}`"))?;
            for rule in listed.rules() {
                let Some(name) = rule.name() else { continue };
                if declared.contains(name) {
                    continue;
                }
                events
                    .remove_targets()
                    .rule(name)
                    .ids(name)
                    .send()
                    .await
                    .with_context(|| format!("failed to clear targets of stale rule `{name}`"))?;
                events
                    .delete_rule()
                    .name(name)
                    .send()
                    .await
                    .with_context(|| format!("failed to delete stale EventBridge rule `{name}`"))?;
                // The permission statement was named after the schedule, which is everything
                // after the function prefix; a rule that outlived the deploy that made it may
                // not have one, so a missing statement is not a failure.
                let schedule = &name[prefix.len()..];
                match lambda
                    .remove_permission()
                    .function_name(&self.function)
                    .statement_id(format!("skyzen-schedule-{schedule}"))
                    .send()
                    .await
                {
                    Ok(_) => {}
                    Err(error)
                        if error.as_service_error().is_some_and(
                            aws_sdk_lambda::operation::remove_permission::RemovePermissionError::is_resource_not_found_exception,
                        ) => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "failed to drop the invoke permission of `{name}` on `{}`",
                                self.function
                            )
                        });
                    }
                }
                output::ok(format!("removed stale EventBridge rule `{name}`"));
            }
            let Some(token) = listed.next_token() else {
                break;
            };
            request = events.list_rules().name_prefix(&prefix).next_token(token);
        }
        Ok(())
    }

    /// Create one `[[schedule]]`'s rule, permission and target.
    async fn wire_one(
        &self,
        events: &aws_sdk_eventbridge::Client,
        lambda: &aws_sdk_lambda::Client,
        arn: &str,
        (region, account): (&str, &str),
        entry: &ScheduleEntry,
    ) -> Result<()> {
        let rule = self.rule_name(entry.name.as_str());
        let expression = aws_cron_expression(entry.cron.as_str()).with_context(|| {
            format!(
                "[[schedule]] `{name}` cannot be expressed as an EventBridge cron",
                name = entry.name
            )
        })?;

        events
            .put_rule()
            .name(&rule)
            .schedule_expression(&expression)
            .state(RuleState::Enabled)
            .send()
            .await
            .with_context(|| format!("failed to create EventBridge rule `{rule}`"))?;

        // The statement already exists only when a previous deploy made it, so `get_policy`
        // decides whether `add_permission` is needed at all: `ResourceConflictException` is
        // ambiguous between "already granted" and "an update is in progress", and swallowing
        // the second case would leave the rule unable to invoke.
        let rule_arn = format!("arn:aws:events:{region}:{account}:rule/{rule}");
        let statement = format!("skyzen-schedule-{}", entry.name);
        if !self
            .has_statement(lambda, &statement, &rule_arn)
            .await
            .with_context(|| {
                format!(
                    "failed to read the invoke policy of Lambda function `{}`",
                    self.function
                )
            })?
        {
            lambda
                .add_permission()
                .function_name(&self.function)
                .action(INVOKE_ACTION)
                .principal(EVENTS_PRINCIPAL)
                .source_arn(&rule_arn)
                .statement_id(&statement)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed to let `{rule}` invoke Lambda function `{}`",
                        self.function
                    )
                })?;
        }

        let transformer = InputTransformer::builder()
            .input_paths_map("time".to_owned(), "$.time".to_owned())
            .input_paths_map("rule".to_owned(), "$.resources[0]".to_owned())
            .input_template(event_template(entry.cron.as_str()))
            .build()
            .with_context(|| format!("could not build the input transform for `{rule}`"))?;
        let put = events
            .put_targets()
            .rule(&rule)
            .targets(
                Target::builder()
                    .id(&rule)
                    .arn(arn)
                    .input_transformer(transformer)
                    .build()
                    .with_context(|| {
                        format!("could not build the EventBridge target for `{rule}`")
                    })?,
            )
            .send()
            .await
            .with_context(|| format!("failed to point EventBridge rule `{rule}` at `{arn}`"))?;
        // `PutTargets` can answer 200 and still reject entries per target — the failed list is
        // the only place that shows up, and printing "wired" over it would lie about the deploy.
        let failures = put.failed_entries();
        anyhow::ensure!(
            failures.is_empty(),
            "EventBridge refused the target of rule `{rule}`: {:?}",
            failures
                .first()
                .map(aws_sdk_eventbridge::types::PutTargetsResultEntry::error_message)
        );

        output::ok(format!(
            "wired `[[schedule]] {name}` to EventBridge rule `{rule}` ({expression})",
            name = entry.name
        ));
        Ok(())
    }

    /// Whether the function's policy already grants `statement` for `source_arn`.
    ///
    /// `GetPolicy` answers 404 (`ResourceNotFoundException`) when the function has no
    /// resource policy at all, which is the common first deploy: no statements, no grants.
    async fn has_statement(
        &self,
        lambda: &aws_sdk_lambda::Client,
        statement: &str,
        source_arn: &str,
    ) -> Result<bool> {
        let policy = match lambda
            .get_policy()
            .function_name(&self.function)
            .send()
            .await
        {
            Ok(response) => response.policy().unwrap_or_default().to_owned(),
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_lambda::operation::get_policy::GetPolicyError::is_resource_not_found_exception,
                ) =>
            {
                return Ok(false)
            }
            Err(error) => return Err(error.into()),
        };

        let parsed: serde_json::Value = serde_json::from_str(&policy)
            .with_context(|| format!("the policy of `{}` is not JSON", self.function))?;
        Ok(parsed["Statement"].as_array().is_some_and(|statements| {
            statements.iter().any(|entry| {
                entry["Sid"] == statement
                    && entry["Condition"]["ArnLike"]["AWS:SourceArn"] == source_arn
            })
        }))
    }
}

/// The event the input transformer stamps, in the shape `skyzen-lambda` decodes.
///
/// `<time>` and `<rule>` are `EventBridge`'s own template paths; `cron` is the portable
/// expression, which the manifest already validated to contain no characters a JSON string
/// needs escaped.
fn event_template(cron: &str) -> String {
    format!(
        "{{\"detail-type\":\"Scheduled Event\",\"source\":\"aws.events\",\
         \"time\":<time>,\"resources\":[<rule>],\"detail\":{{\"cron\":\"{cron}\"}}}}"
    )
}

/// The `cron(...)` schedule expression `EventBridge` wants for a portable 5-field cron.
///
/// `EventBridge` uses AWS's own cron grammar: six fields (the portable five plus a year, always
/// `*`), `?` required in whichever of day-of-month and day-of-week is unconstrained, and
/// `start/n` steps rather than `*/n`. A portable entry constraining *both* day fields has no
/// `EventBridge` form, and fails here naming the entry rather than silently picking one.
///
/// # Errors
///
/// [`anyhow::Error`] when the day-of-month and day-of-week fields are both constrained, or when
/// a field does not map onto AWS's grammar.
fn aws_cron_expression(cron: &str) -> Result<String> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    let [minute, hour, dom, month, dow] = fields
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected five whitespace-separated fields"))?;

    let (dom, dow) = match (dom != "*", dow != "*") {
        (true, true) => anyhow::bail!(
            "AWS requires either day-of-month or day-of-week to be `?`; use one or the other"
        ),
        (_, false) => (dom.to_owned(), "?".to_owned()),
        (false, true) => ("?".to_owned(), dow.to_owned()),
    };

    // `*/n` becomes `{MIN}/n`: AWS steps read as `start/step`, and `0` is not a valid start for
    // the day-of-month, month or day-of-week fields — `*/2` of days is `1/2`, not `0/2`.
    let rendered = [
        aws_field(minute, "0"),
        aws_field(hour, "0"),
        aws_field(&dom, "1"),
        aws_field(month, "1"),
        aws_field(&dow, "1"),
    ];
    Ok(format!("cron({} *)", rendered.join(" ")))
}

/// One AWS cron field: the portable text, with `*/n` spelled `{min}/n`.
///
/// Day-of-week numbers pass through unchanged: portable `1`–`7` is Sunday–Saturday, which is
/// AWS's numbering too.
fn aws_field(field: &str, min: &str) -> String {
    field
        .strip_prefix("*/")
        .map_or_else(|| field.to_owned(), |step| format!("{min}/{step}"))
}

#[cfg(test)]
mod tests {
    use super::{aws_cron_expression, event_template};

    #[test]
    fn a_wildcard_schedule_becomes_all_question_marks() {
        assert_eq!(
            aws_cron_expression("*/10 * * * *").expect("translatable"),
            "cron(0/10 * * * ? *)"
        );
    }

    #[test]
    fn a_day_of_month_schedule_marks_day_of_week_with_a_question() {
        assert_eq!(
            aws_cron_expression("0 9 1 * *").expect("translatable"),
            "cron(0 9 1 * ? *)"
        );
    }

    #[test]
    fn a_day_of_week_schedule_marks_day_of_month_with_a_question() {
        assert_eq!(
            aws_cron_expression("30 12 * * MON-FRI").expect("translatable"),
            "cron(30 12 ? * MON-FRI *)"
        );
    }

    #[test]
    fn steps_start_at_each_fields_minimum_not_zero() {
        assert_eq!(
            aws_cron_expression("0 0 */2 * *").expect("translatable"),
            "cron(0 0 1/2 * ? *)"
        );
        assert_eq!(
            aws_cron_expression("0 0 * */3 */2").expect("translatable"),
            "cron(0 0 ? 1/3 1/2 *)"
        );
        assert_eq!(
            aws_cron_expression("*/15 */3 * * *").expect("translatable"),
            "cron(0/15 0/3 * * ? *)"
        );
    }

    #[test]
    fn numeric_weekdays_keep_their_meaning() {
        // Portable and AWS both count Sunday–Saturday as `1`–`7`.
        assert_eq!(
            aws_cron_expression("0 9 * * 2-6").expect("translatable"),
            "cron(0 9 ? * 2-6 *)"
        );
        assert_eq!(
            aws_cron_expression("0 9 * * 1,4,7").expect("translatable"),
            "cron(0 9 ? * 1,4,7 *)"
        );
    }

    #[test]
    fn constraining_both_day_fields_is_refused() {
        assert!(aws_cron_expression("0 9 1 * MON").is_err());
    }

    #[test]
    fn the_template_carries_the_portable_cron_where_the_runtime_reads_it() {
        let template = event_template("*/10 * * * *");
        let rendered: serde_json::Value = serde_json::from_str(
            &template
                .replace("<time>", "\"2025-01-01T00:00:00Z\"")
                .replace("<rule>", "\"arn:aws:events:us-east-1:123:rule/x\""),
        )
        .expect("the template renders valid JSON");
        assert_eq!(rendered["detail-type"], "Scheduled Event");
        assert_eq!(rendered["source"], "aws.events");
        assert_eq!(rendered["detail"]["cron"], "*/10 * * * *");
    }
}
