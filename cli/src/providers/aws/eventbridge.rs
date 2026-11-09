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
use aws_sdk_lambda::client::Waiters as _;
use skyzen_manifest::ScheduleEntry;

/// The Lambda resource-based permission a rule needs to invoke the function.
const INVOKE_ACTION: &str = "lambda:InvokeFunction";

/// The `EventBridge` service principal.
const EVENTS_PRINCIPAL: &str = "events.amazonaws.com";

/// The alias every rule's target carries: invocations through it change only when the deploy
/// repoints it at the new version — atomically, unlike any rule-state change EventBridge can
/// propagate.
const ALIAS: &str = "skyzen";

/// What a rule looked like before this deploy touched it: enough to put it back.
#[derive(Debug)]
struct RuleSnapshot {
    /// The rule's name.
    name: String,
    /// Its schedule expression, if it had one.
    expression: Option<String>,
    /// Whether it was enabled.
    enabled: bool,
    /// The targets it carried.
    targets: Vec<Target>,
}

/// The rules that point `[[schedule]]` entries at a deployed function.
#[derive(Debug)]
pub struct EventBridgeSchedules {
    /// The function to target, as `deploy` names it.
    function: String,
    /// The `[[schedule]]` entries to wire, each becoming one rule.
    schedules: Vec<ScheduleEntry>,
    /// The environment to deliver while the rules are suspended — empty delivers nothing.
    delivery: crate::providers::secrets::Delivery,
    /// The deploy's `child_env` — where `.env` AWS credentials ride to the SDK.
    env: Vec<(String, String)>,
}

impl EventBridgeSchedules {
    /// The step that wires `schedules` to `function`.
    pub const fn new(
        function: String,
        schedules: Vec<ScheduleEntry>,
        delivery: crate::providers::secrets::Delivery,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            function,
            schedules,
            delivery,
            env,
        }
    }

    /// The rule `name` becomes when pointed at this function.
    ///
    /// `skyzen-` namespaces the rule from anything an account made by hand or another tool,
    /// and the double dash keeps the join unambiguous: `foo`/`bar-baz` and `foo-bar`/`baz`
    /// name different rules.
    fn rule_name(&self, name: &str) -> String {
        format!("skyzen-{}--{name}", self.function)
    }
}

impl Task for EventBridgeSchedules {
    fn describe(&self) -> String {
        let delivery = if self.delivery.is_empty() {
            String::new()
        } else {
            format!(", and deliver its environment: {}", self.delivery.names())
        };
        if self.schedules.is_empty() {
            return format!(
                "reconcile `[[schedule]]` EventBridge rules of Lambda function `{}`{delivery}",
                self.function
            );
        }
        let names = self
            .schedules
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "wire {} EventBridge schedule(s) to Lambda function `{}`: {names}{delivery}",
            self.schedules.len(),
            self.function,
        )
    }

    fn run(&self) -> Result<()> {
        super::lambda_env::adopt_env(&self.env);
        block_on(async {
            let shared = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let lambda = aws_sdk_lambda::Client::new(&shared);
            let events = aws_sdk_eventbridge::Client::new(&shared);

            // The deploy that just ran leaves an update in progress — `add_permission` meets
            // it as `ResourceConflictException`. Settle the update first, the same wait the
            // environment-delivery step performs.
            lambda
                .wait_until_function_updated_v2()
                .function_name(&self.function)
                .wait(super::lambda_env::UPDATE_TIMEOUT)
                .await
                .with_context(|| {
                    format!(
                        "Lambda function `{}` did not finish updating within {} seconds",
                        self.function,
                        super::lambda_env::UPDATE_TIMEOUT.as_secs()
                    )
                })?;

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

            // `cargo lambda deploy` has already swapped the code, and until the environment
            // lands an enabled rule could fire it on stale configuration. The rules point at
            // the `skyzen` alias, so the transition itself is what makes them safe: the new
            // version publishes once the environment is in place, and the alias's atomic
            // repoint is the moment their invocations change.
            let mut snapshot = Vec::new();
            let result = self.reconcile(&events, &lambda, &arn, &mut snapshot).await;
            if let Err(error) = result {
                for taken in &snapshot {
                    let _ = self.restore_rule(&events, taken).await;
                }
                return Err(error);
            }
            Ok(())
        })?
    }
}

impl EventBridgeSchedules {
    /// Deliver the environment, publish the version, repoint the alias, then wire and retire.
    ///
    /// `snapshot` captures every rule before it changes so the caller can put it back — a
    /// failed deployment must not leave schedules disabled that were enabled, or delete
    /// wiring a stale rule still carried.
    async fn reconcile(
        &self,
        events: &aws_sdk_eventbridge::Client,
        lambda: &aws_sdk_lambda::Client,
        arn: &str,
        snapshot: &mut Vec<RuleSnapshot>,
    ) -> Result<()> {
        if !self.delivery.is_empty() {
            super::lambda_env::LambdaEnvironment::new(
                self.function.clone(),
                self.delivery.clone(),
                self.env.clone(),
            )
            .deliver()
            .await?;
        }

        // The version captures the code and configuration $LATEST now holds; the rules
        // target the alias, so nothing they invoke changes until the repoint below — the
        // only barrier EventBridge propagation cannot undermine.
        let version = lambda
            .publish_version()
            .function_name(&self.function)
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to publish a version of Lambda function `{}`",
                    self.function
                )
            })?
            .version()
            .with_context(|| format!("`{}` published without a version", self.function))?
            .to_owned();
        self.point_the_alias(lambda, &version).await?;
        let alias_arn = format!("{arn}:{ALIAS}");
        let qualified = format!("{}:{ALIAS}", self.function);

        // arn:{partition}:lambda:{region}:{account}:function:{name} — the rule's ARN lives
        // in the same partition, region and account, under the `events` service. The
        // partition matters outside standard AWS: `aws-us-gov`, `aws-cn`.
        let parts: Vec<&str> = arn.split(':').collect();
        let (partition, region, account) = parts
            .get(1)
            .zip(parts.get(3))
            .zip(parts.get(4))
            .map(|((partition, region), account)| (*partition, *region, *account))
            .with_context(|| {
                format!("`{arn}` is not a Lambda function ARN EventBridge rules can be built from")
            })?;

        for entry in &self.schedules {
            if let Some(taken) = self
                .rule_snapshot(events, &self.rule_name(entry.name.as_str()))
                .await?
            {
                snapshot.push(taken);
            }
            self.wire_one(
                events,
                lambda,
                &qualified,
                &alias_arn,
                (partition, region, account),
                entry,
            )
            .await?;
        }
        self.retire_stale(events, lambda, &qualified, &alias_arn, snapshot)
            .await?;
        Ok(())
    }

    /// What a rule looked like before this deploy touched it, or `None` when it does not
    /// exist. A lookup failure other than "not there" proves nothing about the rule — it is
    /// an error, not an absence.
    async fn rule_snapshot(
        &self,
        events: &aws_sdk_eventbridge::Client,
        rule: &str,
    ) -> Result<Option<RuleSnapshot>> {
        let found = match events.describe_rule().name(rule).send().await {
            Ok(found) => found,
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_eventbridge::operation::describe_rule::DescribeRuleError::is_resource_not_found_exception,
                ) =>
            {
                return Ok(None)
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to describe rule `{rule}`"));
            }
        };
        let targets = events
            .list_targets_by_rule()
            .rule(rule)
            .send()
            .await
            .with_context(|| format!("failed to list the targets of rule `{rule}`"))?
            .targets()
            .to_vec();
        Ok(Some(RuleSnapshot {
            name: rule.to_owned(),
            expression: found.schedule_expression().map(str::to_owned),
            enabled: matches!(
                found.state(),
                Some(aws_sdk_eventbridge::types::RuleState::Enabled)
            ),
            targets,
        }))
    }

    /// Put a snapshotted rule back: its schedule, its targets, its enabled state.
    async fn restore_rule(
        &self,
        events: &aws_sdk_eventbridge::Client,
        snapshot: &RuleSnapshot,
    ) -> Result<()> {
        let mut put = events
            .put_rule()
            .name(&snapshot.name)
            .state(if snapshot.enabled {
                aws_sdk_eventbridge::types::RuleState::Enabled
            } else {
                aws_sdk_eventbridge::types::RuleState::Disabled
            });
        if let Some(expression) = &snapshot.expression {
            put = put.schedule_expression(expression);
        }
        put.send()
            .await
            .with_context(|| format!("failed to restore rule `{}`", snapshot.name))?;
        if !snapshot.targets.is_empty() {
            events
                .put_targets()
                .rule(&snapshot.name)
                .set_targets(Some(snapshot.targets.clone()))
                .send()
                .await
                .with_context(|| format!("failed to restore targets of `{}`", snapshot.name))?;
        }
        Ok(())
    }

    /// Point the `skyzen` alias at `version`, creating the alias the first time.
    async fn point_the_alias(&self, lambda: &aws_sdk_lambda::Client, version: &str) -> Result<()> {
        let update = lambda
            .update_alias()
            .function_name(&self.function)
            .name(ALIAS)
            .function_version(version)
            .send()
            .await;
        match update {
            Ok(_) => {}
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_lambda::operation::update_alias::UpdateAliasError::is_resource_not_found_exception,
                ) =>
            {
                lambda
                    .create_alias()
                    .function_name(&self.function)
                    .name(ALIAS)
                    .function_version(version)
                    .send()
                    .await
                    .with_context(|| {
                        format!(
                            "failed to create the `{ALIAS}` alias of Lambda function `{}`",
                            self.function
                        )
                    })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to repoint the `{ALIAS}` alias of Lambda function `{}`",
                        self.function
                    )
                });
            }
        }
        output::ok(format!(
            "pointed the `{ALIAS}` alias of Lambda function `{}` at version `{version}`",
            self.function
        ));
        Ok(())
    }

    /// Remove rules — and their invoke permissions — that a previous deploy created and the
    /// manifest no longer declares.
    ///
    /// The function's resource policy is the record: every statement `skyzen-schedule-{name}`
    /// is a grant this module made, so a grant whose name the manifest no longer declares is
    /// unambiguously stale. The matching rule and permission are retired together.
    async fn retire_stale(
        &self,
        events: &aws_sdk_eventbridge::Client,
        lambda: &aws_sdk_lambda::Client,
        qualified: &str,
        arn: &str,
        snapshot: &mut Vec<RuleSnapshot>,
    ) -> Result<()> {
        const STATEMENT_PREFIX: &str = "skyzen-schedule-";

        let Some(policy) = self.policy(lambda, qualified).await? else {
            return Ok(());
        };
        let declared: std::collections::BTreeSet<&str> = self
            .schedules
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        let stale = policy["Statement"].as_array().map(|statements| {
            statements
                .iter()
                .filter_map(|entry| entry["Sid"].as_str()?.strip_prefix(STATEMENT_PREFIX))
                .filter(|name| !declared.contains(name))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let Some(stale) = stale else {
            return Ok(());
        };

        for name in stale {
            let rule = self.rule_name(&name);
            // The delete is one-way — the rule's own record is what a failed deploy restores.
            if let Some(taken) = self.rule_snapshot(events, &rule).await? {
                snapshot.push(taken);
            }
            self.retire_rule(events, arn, &rule).await?;
            match lambda
                .remove_permission()
                .function_name(qualified)
                .statement_id(format!("{STATEMENT_PREFIX}{name}"))
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
                            "failed to drop the invoke permission of `{rule}` on `{}`",
                            self.function
                        )
                    });
                }
            }
            output::ok(format!("removed stale EventBridge rule `{rule}`"));
        }
        Ok(())
    }

    /// Delete `rule` after stripping every target on it.
    ///
    /// `DeleteRule` refuses while any target remains. Enumerated rather than assumed, and
    /// `RemoveTargets`'s per-entry failures fail the deploy the way `PutTargets`'s do. A rule
    /// that vanished between the policy read and here is not a failure — and neither is a rule
    /// carrying a target that is not this Lambda: another integration attached one, and
    /// removing it would take down somebody else's wiring, so the rule stays and says so.
    async fn retire_rule(
        &self,
        events: &aws_sdk_eventbridge::Client,
        arn: &str,
        rule: &str,
    ) -> Result<()> {
        let listed = match events.list_targets_by_rule().rule(rule).send().await {
            Ok(listed) => listed,
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_eventbridge::operation::list_targets_by_rule::ListTargetsByRuleError::is_resource_not_found_exception,
                ) =>
            {
                return Ok(())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to list the targets of rule `{rule}`"));
            }
        };
        if let Some(target) = listed.targets().iter().find(|target| target.arn() != arn) {
            output::warn(format!(
                "leaving stale EventBridge rule `{rule}` in place: it carries a target pointing \
                 at `{}`, which is not Lambda function `{}`",
                target.arn(),
                self.function
            ));
            return Ok(());
        }
        let ids: Vec<String> = listed
            .targets()
            .iter()
            .map(|target| target.id().to_owned())
            .collect();
        if !ids.is_empty() {
            let removed = events
                .remove_targets()
                .rule(rule)
                .set_ids(Some(ids))
                .send()
                .await
                .with_context(|| format!("failed to clear targets of stale rule `{rule}`"))?;
            let failures = removed.failed_entries();
            anyhow::ensure!(
                failures.is_empty(),
                "EventBridge refused to strip a target of stale rule `{rule}`: {:?}",
                failures
                    .first()
                    .map(aws_sdk_eventbridge::types::RemoveTargetsResultEntry::error_message)
            );
        }
        events
            .delete_rule()
            .name(rule)
            .send()
            .await
            .with_context(|| format!("failed to delete stale EventBridge rule `{rule}`"))?;
        Ok(())
    }

    /// The function's resource policy document, or `None` when it has none.
    async fn policy(
        &self,
        lambda: &aws_sdk_lambda::Client,
        qualified: &str,
    ) -> Result<Option<serde_json::Value>> {
        let policy = match lambda
            .get_policy()
            .function_name(qualified)
            .send()
            .await
        {
            Ok(response) => response.policy().unwrap_or_default().to_owned(),
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_lambda::operation::get_policy::GetPolicyError::is_resource_not_found_exception,
                ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error.into()),
        };
        let parsed: serde_json::Value = serde_json::from_str(&policy)
            .with_context(|| format!("the policy of `{}` is not JSON", self.function))?;
        Ok(Some(parsed))
    }

    /// Refuse to touch `rule` when something else owns it.
    ///
    /// A rule with that name that already points at this function — or has no targets at all —
    /// is one an earlier Skyzen deploy made, safe to put over. A rule carrying a target that
    /// is not this Lambda is somebody else's, and `put_rule`/`put_targets` would silently
    /// retarget it.
    async fn own_the_rule(
        &self,
        events: &aws_sdk_eventbridge::Client,
        arn: &str,
        rule: &str,
    ) -> Result<()> {
        match events.describe_rule().name(rule).send().await {
            Ok(_) => {}
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_eventbridge::operation::describe_rule::DescribeRuleError::is_resource_not_found_exception,
                ) =>
            {
                return Ok(())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read EventBridge rule `{rule}`"))
            }
        }

        let listed = events
            .list_targets_by_rule()
            .rule(rule)
            .send()
            .await
            .with_context(|| format!("failed to list the targets of EventBridge rule `{rule}`"))?;
        if let Some(target) = listed.targets().iter().find(|target| target.arn() != arn) {
            anyhow::bail!(
                "EventBridge rule `{rule}` already exists and targets `{}`, not Lambda function \
                 `{}`; rename the schedule or remove the foreign rule",
                target.arn(),
                self.function
            );
        }

        // Same-ARN targets with different ids are still distinct targets — `put_targets` adds
        // rather than replaces, and EventBridge would invoke the function once per target.
        // Strip every id but the one `wire_one` is about to write.
        let surplus: Vec<String> = listed
            .targets()
            .iter()
            .map(|target| target.id().to_owned())
            .filter(|id| id != rule)
            .collect();
        if !surplus.is_empty() {
            let removed = events
                .remove_targets()
                .rule(rule)
                .set_ids(Some(surplus))
                .send()
                .await
                .with_context(|| format!("failed to prune targets of rule `{rule}`"))?;
            let failures = removed.failed_entries();
            anyhow::ensure!(
                failures.is_empty(),
                "EventBridge refused to strip a target of rule `{rule}`: {:?}",
                failures
                    .first()
                    .map(aws_sdk_eventbridge::types::RemoveTargetsResultEntry::error_message)
            );
        }
        Ok(())
    }

    /// Create one `[[schedule]]`'s rule, permission and target.
    async fn wire_one(
        &self,
        events: &aws_sdk_eventbridge::Client,
        lambda: &aws_sdk_lambda::Client,
        qualified: &str,
        arn: &str,
        (partition, region, account): (&str, &str, &str),
        entry: &ScheduleEntry,
    ) -> Result<()> {
        let rule = self.rule_name(entry.name.as_str());
        anyhow::ensure!(
            rule.len() <= 64,
            "the EventBridge rule `{rule}` is {} characters; AWS allows 64 — shorten the \
             schedule or function name",
            rule.len()
        );
        let expression = aws_cron_expression(entry.cron.as_str()).with_context(|| {
            format!(
                "[[schedule]] `{name}` cannot be expressed as an EventBridge cron",
                name = entry.name
            )
        })?;

        self.own_the_rule(events, arn, &rule).await?;

        // The rule goes up disabled and is switched on only once its permission and target
        // are both in place — a rule enabled before its transformer reconciles can fire the
        // function with the previous deployment's `detail.cron`.
        events
            .put_rule()
            .name(&rule)
            .schedule_expression(&expression)
            .state(RuleState::Disabled)
            .send()
            .await
            .with_context(|| format!("failed to create EventBridge rule `{rule}`"))?;

        // The statement already exists only when a previous deploy made it, so `get_policy`
        // decides whether `add_permission` is needed at all: `ResourceConflictException` is
        // ambiguous between "already granted" and "an update is in progress", and swallowing
        // the second case would leave the rule unable to invoke.
        let rule_arn = format!("arn:{partition}:events:{region}:{account}:rule/{rule}");
        let statement = format!("skyzen-schedule-{}", entry.name);
        if !self
            .has_statement(lambda, qualified, &statement, &rule_arn)
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
                .function_name(qualified)
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

        // Everything the rule needs — permission, target, transformer — is in place; only now
        // does it switch on.
        events
            .enable_rule()
            .name(&rule)
            .send()
            .await
            .with_context(|| format!("failed to enable EventBridge rule `{rule}`"))?;

        output::ok(format!(
            "wired `[[schedule]] {name}` to EventBridge rule `{rule}` ({expression})",
            name = entry.name
        ));
        Ok(())
    }

    /// Whether the function's policy already grants `statement` for `source_arn`.
    ///
    /// The whole grant is compared — `Sid` alone is only the name: a statement that matches but
    /// denies, or allows another principal or action, is not the permission `add_permission`
    /// would create, and skipping the grant on its account would leave the rule unable to
    /// invoke.
    async fn has_statement(
        &self,
        lambda: &aws_sdk_lambda::Client,
        qualified: &str,
        statement: &str,
        source_arn: &str,
    ) -> Result<bool> {
        let Some(policy) = self.policy(lambda, qualified).await? else {
            return Ok(false);
        };
        Ok(policy["Statement"].as_array().is_some_and(|statements| {
            statements.iter().any(|entry| {
                entry["Sid"] == statement
                    && entry["Effect"] == "Allow"
                    && entry["Principal"]["Service"] == EVENTS_PRINCIPAL
                    && entry["Action"] == INVOKE_ACTION
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
/// `pub(super)` so `prepare` can reject an expression `EventBridge` cannot take before a
/// deploy step uploads the Lambda and only then discovers it.
pub(super) fn aws_cron_expression(cron: &str) -> Result<String> {
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
