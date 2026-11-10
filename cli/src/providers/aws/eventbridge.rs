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
    /// Whether the rule existed at all — a deploy-made rule that never was gets deleted.
    existed: bool,
    /// Its schedule expression, if it had one.
    expression: Option<String>,
    /// Whether it was enabled.
    enabled: bool,
    /// The targets it carried.
    targets: Vec<Target>,
    /// The policy statement that let it invoke, if one existed — removal is reversible only
    /// when the grant comes back with the rule.
    statement: Option<String>,
    /// The function (`fn` or `fn:skyzen`) the grant lived on — a legacy rule restores on
    /// the unqualified name, not the alias.
    owner: Option<String>,
    /// Whether the deploy added the grant — a created permission is rollback's to remove.
    granted: bool,
    /// Whether the transaction actually changed the rule — a snapshot whose ownership check
    /// rejected the rule was never mutated, and restoring it would be the mutation.
    mutated: bool,
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

/// Retarget managed rules at the `skyzen` alias before new code uploads.
///
/// An earlier deploy pointed every rule at the unqualified function, which fires the moment
/// `cargo lambda deploy` swaps `$LATEST` — before the environment lands. This step publishes
/// the *current* code and configuration as a version, points the alias at it and moves every
/// managed rule's targets onto the alias. From then on the transition is the repoint inside
/// [`EventBridgeSchedules`]; a build that fails afterwards still leaves every rule on a
/// consistent old version.
#[derive(Debug)]
pub struct MigrateSchedulesToAlias {
    /// The rules' owner, reusing `EventBridgeSchedules`' naming and policy reading.
    inner: EventBridgeSchedules,
}

impl MigrateSchedulesToAlias {
    /// The step that moves `schedules` plus every `skyzen-schedule-*` grant's rule.
    pub fn new(
        function: String,
        schedules: Vec<ScheduleEntry>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            inner: EventBridgeSchedules::new(
                function,
                schedules,
                crate::providers::secrets::Delivery::default(),
                env,
            ),
        }
    }
}

impl Task for MigrateSchedulesToAlias {
    fn describe(&self) -> String {
        format!(
            "point `[[schedule]]` EventBridge rules of Lambda function `{}` at the `{ALIAS}` \
             alias",
            self.inner.function
        )
    }

    fn run(&self) -> Result<()> {
        // A manifest without schedules has nothing to migrate — and no call for the
        // schedule-management permissions the migration would need.
        if self.inner.schedules.is_empty() {
            return Ok(());
        }
        super::lambda_env::adopt_env(&self.inner.env);
        block_on(async {
            let shared = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let lambda = aws_sdk_lambda::Client::new(&shared);
            let events = aws_sdk_eventbridge::Client::new(&shared);
            self.inner.migrate_to_alias(&events, &lambda).await
        })?
    }
}

/// Repoint the `skyzen` alias at a version holding the environment `deliver` just wrote.
///
/// Secrets deliver to `$LATEST` — without this step, schedules keep invoking the version
/// the alias already points at and the rotation never reaches them.
#[derive(Debug)]
pub struct CutoverAlias {
    /// The alias owner, reusing `EventBridgeSchedules`' publish and repoint.
    inner: EventBridgeSchedules,
}

impl CutoverAlias {
    /// The step that moves `skyzen` to the freshest published version of `function`.
    pub fn new(function: String, env: Vec<(String, String)>) -> Self {
        Self {
            inner: EventBridgeSchedules::new(
                function,
                Vec::new(),
                crate::providers::secrets::Delivery::default(),
                env,
            ),
        }
    }
}

impl Task for CutoverAlias {
    fn describe(&self) -> String {
        format!(
            "point the `{ALIAS}` alias of Lambda function `{}` at the updated environment",
            self.inner.function
        )
    }

    fn run(&self) -> Result<()> {
        super::lambda_env::adopt_env(&self.inner.env);
        block_on(async {
            let shared = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let lambda = aws_sdk_lambda::Client::new(&shared);
            // An app that never scheduled has no alias — and no call for the permissions a
            // repoint would take. Only a confirmed absence skips the cutover: the delivery
            // just happened, and every other answer leaves the schedules holding old values
            // — report it rather than pretend the rotation finished.
            match self.inner.alias_snapshot(&lambda).await {
                Ok(Some(_)) => {}
                Ok(None) => return Ok(()),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to read the `{ALIAS}` alias of `{}` after delivering \
                             secrets — the scheduled environment is unchanged",
                            self.inner.function
                        )
                    });
                }
            }
            // `$LATEST` just took the new values; a version freezes them so the alias's
            // repoint is atomic — nothing invokes a half-rotated environment.
            let version = self.inner.publish_or_reuse(&lambda).await?;
            self.inner.point_the_alias(&lambda, &version).await
        })?
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
            // Restore needs what reconcile would have built: the qualified function the
            // grants live on, and the ARN parts a rule's own ARN derives from.
            let parts: Vec<&str> = arn.split(':').collect();
            let (partition, region, account) = parts
                .get(1)
                .zip(parts.get(3))
                .zip(parts.get(4))
                .map(|((partition, region), account)| (*partition, *region, *account))
                .with_context(|| {
                    format!(
                        "`{arn}` is not a Lambda function ARN EventBridge rules can be built from"
                    )
                })?;
            let qualified = format!("{}:{ALIAS}", self.function);
            let mut snapshot = Vec::new();
            let mut alias = None;
            let result = self
                .reconcile(
                    &events,
                    &lambda,
                    &arn,
                    (partition, region, account),
                    &mut snapshot,
                    &mut alias,
                )
                .await;
            if let Err(error) = result {
                // The alias goes back first: rules re-enabled by the restore below must
                // find the old version behind them, or the rollback itself wakes a stale
                // cron on new code.
                if let Some(previous) = &alias {
                    let restore = lambda
                        .update_alias()
                        .function_name(&self.function)
                        .name(ALIAS)
                        .function_version(previous)
                        .send()
                        .await;
                    if let Err(restore) = restore {
                        output::warn(format!(
                            "could not repoint the `{ALIAS}` alias back at `{previous}`: \
                             {restore}"
                        ));
                    }
                }
                for taken in &snapshot {
                    let rule_arn = format!(
                        "arn:{partition}:events:{region}:{account}:rule/{}",
                        taken.name
                    );
                    if let Err(restore) = self
                        .restore_rule(&events, &lambda, &qualified, &rule_arn, taken)
                        .await
                    {
                        output::warn(format!(
                            "could not restore EventBridge rule `{}`: {restore}",
                            taken.name
                        ));
                    }
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
        (partition, region, account): (&str, &str, &str),
        snapshot: &mut Vec<RuleSnapshot>,
        alias: &mut Option<String>,
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
        let alias_arn = format!("{arn}:{ALIAS}");
        let qualified = format!("{}:{ALIAS}", self.function);
        // A manifest with no schedules still carries the environment and may still find
        // stale grants to retire — but an HTTP-only app need not hold the permissions that
        // cleanup wants. Its discovery is best-effort: whatever it cannot read, it leaves.
        if self.schedules.is_empty() {
            // An HTTP-only app may hold none of the permissions cleanup wants — discovering
            // it has no managed rules is enough to leave everything alone. But a failure
            // while managed rules were actually found must stop the deploy, or the
            // supposedly removed schedules keep firing.
            match self.managed_rule_names(events).await {
                Ok(rules) if rules.is_empty() => {}
                Ok(rules) => {
                    self.retire_stale(events, lambda, &qualified, arn, &alias_arn, snapshot)
                        .await?;
                }
                Err(error) => {
                    output::warn(format!("could not check for stale schedules: {error:#}"));
                }
            }
            return Ok(());
        }

        let version = self.publish_or_reuse(lambda).await?;
        // The alias must exist before any `add_permission` can name it — on a first deploy
        // it does not — but it repoints only after the rules are reconciled. Create it at
        // the published version here; the final `point_the_alias` below is the cutover.
        if self.alias_snapshot(lambda).await?.is_none() {
            lambda
                .create_alias()
                .function_name(&self.function)
                .name(ALIAS)
                .function_version(&version)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed to create the `{ALIAS}` alias of Lambda function `{}`",
                        self.function
                    )
                })?;
        }

        for entry in &self.schedules {
            // Absent rules snapshot too: a rule the deploy creates belongs to the rollback,
            // or a failed deployment leaves enabled wiring behind.
            let mut taken = self
                .rule_snapshot(events, &self.rule_name(entry.name.as_str()))
                .await?;
            snapshot.push(taken);
            // `wire_one` can fail partway — the snapshot must already be on the stack, or
            // the half-wired rule survives the rollback it should have been part of.
            let index = snapshot.len() - 1;
            let (mut mutated, mut granted) = (false, false);
            let result = self
                .wire_one(
                    events,
                    lambda,
                    &qualified,
                    &alias_arn,
                    (partition, region, account),
                    entry,
                    &mut mutated,
                    &mut granted,
                )
                .await;
            let taken = &mut snapshot[index];
            taken.mutated = mutated;
            taken.granted = granted;
            result?;
        }
        self.retire_stale(events, lambda, &qualified, arn, &alias_arn, snapshot)
            .await?;
        // The cutover is the last thing to move: every rule is wired disabled, so nothing
        // fires while the transition is open — then the alias repoints once and the
        // schedules switch on behind it, all at once.
        *alias = self.alias_snapshot(lambda).await?;
        self.point_the_alias(lambda, &version).await?;
        for entry in &self.schedules {
            let rule = self.rule_name(entry.name.as_str());
            events
                .enable_rule()
                .name(&rule)
                .send()
                .await
                .with_context(|| format!("failed to enable EventBridge rule `{rule}`"))?;
        }
        Ok(())
    }

    /// What a rule looked like before this deploy touched it, or `None` when it does not
    /// exist. A lookup failure other than "not there" proves nothing about the rule — it is
    /// an error, not an absence.
    async fn rule_snapshot(
        &self,
        events: &aws_sdk_eventbridge::Client,
        rule: &str,
    ) -> Result<RuleSnapshot> {
        let found = match events.describe_rule().name(rule).send().await {
            Ok(found) => found,
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_eventbridge::operation::describe_rule::DescribeRuleError::is_resource_not_found_exception,
                ) =>
            {
                return Ok(RuleSnapshot {
                    name: rule.to_owned(),
                    existed: false,
                    expression: None,
                    enabled: false,
                    targets: Vec::new(),
                    statement: None,
                    owner: None,
                    granted: false,
                    mutated: false,
                })
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
        Ok(RuleSnapshot {
            name: rule.to_owned(),
            existed: true,
            expression: found.schedule_expression().map(str::to_owned),
            enabled: matches!(
                found.state(),
                Some(aws_sdk_eventbridge::types::RuleState::Enabled)
            ),
            targets,
            statement: None,
            owner: None,
            granted: false,
            mutated: false,
        })
    }

    /// Put a snapshotted rule back: a rule that never existed gets deleted, and an existing
    /// one gets its schedule, its targets, its invoke grant and its enabled state back.
    async fn restore_rule(
        &self,
        events: &aws_sdk_eventbridge::Client,
        lambda: &aws_sdk_lambda::Client,
        qualified: &str,
        rule_arn: &str,
        snapshot: &RuleSnapshot,
    ) -> Result<()> {
        // A rule the transaction never changed must not be changed back — restoring an
        // ownership-rejected rule would mutate somebody else's wiring.
        if snapshot.existed && !snapshot.mutated {
            return Ok(());
        }
        if !snapshot.existed {
            let listed = events
                .list_targets_by_rule()
                .rule(&snapshot.name)
                .send()
                .await;
            if let Ok(listed) = listed {
                let ids: Vec<String> = listed
                    .targets()
                    .iter()
                    .map(|target| target.id().to_owned())
                    .collect();
                if !ids.is_empty() {
                    let _ = events
                        .remove_targets()
                        .rule(&snapshot.name)
                        .set_ids(Some(ids))
                        .send()
                        .await;
                }
            }
            events
                .delete_rule()
                .name(&snapshot.name)
                .send()
                .await
                .with_context(|| format!("failed to remove rule `{}`", snapshot.name))?;
            // The grant the wiring added goes with it — a failed deploy must not leave
            // invoke permissions behind for a rule that never was. `name` is the rule
            // `skyzen-{function}--{schedule}` — the grant is `skyzen-schedule-{schedule}`.
            if snapshot.granted {
                let schedule = snapshot
                    .name
                    .strip_prefix(&format!("skyzen-{}--", self.function))
                    .unwrap_or(&snapshot.name);
                let _ = lambda
                    .remove_permission()
                    .function_name(qualified)
                    .statement_id(format!("skyzen-schedule-{schedule}"))
                    .send()
                    .await;
            }
            return Ok(());
        }
        // The rule goes back disabled regardless of its original state — it is enabled last,
        // after its targets are restored, so the window between the recreate and the target
        // fix cannot fire the rule with the failed deploy's transformer.
        let mut put = events
            .put_rule()
            .name(&snapshot.name)
            .state(aws_sdk_eventbridge::types::RuleState::Disabled);
        if let Some(expression) = &snapshot.expression {
            put = put.schedule_expression(expression);
        }
        put.send()
            .await
            .with_context(|| format!("failed to restore rule `{}`", snapshot.name))?;
        // `PutTargets` merges — a target id the wiring introduced outlives the restore.
        // Strip whatever the snapshot does not carry before putting its targets back.
        let listed = events
            .list_targets_by_rule()
            .rule(&snapshot.name)
            .send()
            .await
            .with_context(|| format!("failed to list targets of `{}`", snapshot.name))?;
        let kept: std::collections::BTreeSet<&str> =
            snapshot.targets.iter().map(|target| target.id()).collect();
        let surplus: Vec<String> = listed
            .targets()
            .iter()
            .filter(|target| !kept.contains(target.id()))
            .map(|target| target.id().to_owned())
            .collect();
        if !surplus.is_empty() {
            let removed = events
                .remove_targets()
                .rule(&snapshot.name)
                .set_ids(Some(surplus))
                .send()
                .await
                .with_context(|| format!("failed to strip targets of `{}`", snapshot.name))?;
            anyhow::ensure!(
                removed.failed_entries().is_empty(),
                "EventBridge refused to strip a target of `{}`: {:?}",
                snapshot.name,
                removed
                    .failed_entries()
                    .first()
                    .map(aws_sdk_eventbridge::types::RemoveTargetsResultEntry::error_message)
            );
        }
        if !snapshot.targets.is_empty() {
            let put = events
                .put_targets()
                .rule(&snapshot.name)
                .set_targets(Some(snapshot.targets.clone()))
                .send()
                .await
                .with_context(|| format!("failed to restore targets of `{}`", snapshot.name))?;
            anyhow::ensure!(
                put.failed_entries().is_empty(),
                "EventBridge refused to restore a target of `{}`: {:?}",
                snapshot.name,
                put.failed_entries()
                    .first()
                    .map(aws_sdk_eventbridge::types::PutTargetsResultEntry::error_message)
            );
        }
        // `granted` alone means the deploy added the grant — remove what it added. The
        // `statement`/`owner` pair means retirement removed a grant that existed before —
        // re-add it on the resource it lived on, nowhere else. A grant that was already
        // there and never removed needs nothing.
        if snapshot.granted {
            // `name` is the rule `skyzen-{function}--{schedule}` — the grant it added is
            // `skyzen-schedule-{schedule}` on the alias.
            let schedule = snapshot
                .name
                .strip_prefix(&format!("skyzen-{}--", self.function))
                .unwrap_or(&snapshot.name);
            let _ = lambda
                .remove_permission()
                .function_name(qualified)
                .statement_id(format!("skyzen-schedule-{schedule}"))
                .send()
                .await;
        } else if let Some(statement) = &snapshot.statement {
            lambda
                .add_permission()
                .function_name(snapshot.owner.as_deref().unwrap_or(qualified))
                .action(INVOKE_ACTION)
                .principal(EVENTS_PRINCIPAL)
                .source_arn(rule_arn)
                .statement_id(statement)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed to restore the invoke permission of `{}`",
                        snapshot.name
                    )
                })?;
        }
        // The state is last: rule, targets and grant are all original again before the
        // rule is allowed to fire.
        if snapshot.enabled {
            events
                .enable_rule()
                .name(&snapshot.name)
                .send()
                .await
                .with_context(|| format!("failed to re-enable rule `{}`", snapshot.name))?;
        }
        Ok(())
    }

    /// Publish $LATEST as a version, reusing the newest existing one when nothing changed.
    ///
    /// `PublishVersion` refuses when `$LATEST` already equals the last version — an empty
    /// delivery and an unchanged function land there, and both still need a version the
    /// alias can point at. The list call paginates, so the newest is the maximum across
    /// every page rather than the first.
    async fn publish_or_reuse(&self, lambda: &aws_sdk_lambda::Client) -> Result<String> {
        match lambda
            .publish_version()
            .function_name(&self.function)
            .send()
            .await
        {
            Ok(published) => published
                .version()
                .with_context(|| format!("`{}` published without a version", self.function))
                .map(str::to_owned),
            Err(error) => {
                if !error.as_service_error().is_some_and(
                    aws_sdk_lambda::operation::publish_version::PublishVersionError::is_precondition_failed_exception,
                ) {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to publish a version of Lambda function `{}`",
                            self.function
                        )
                    });
                }
                let mut newest: Option<String> = None;
                let mut marker: Option<String> = None;
                loop {
                    let mut list = lambda
                        .list_versions_by_function()
                        .function_name(&self.function);
                    if let Some(next) = &marker {
                        list = list.marker(next);
                    }
                    let page = list.send().await.with_context(|| {
                        format!("failed to list versions of `{}`", self.function)
                    })?;
                    for version in page.versions().iter().filter_map(|v| v.version()) {
                        if version != "$LATEST" {
                            // Version numbers compare numerically — "9" sorts after "10"
                            // as text, and only the true newest is safe to point at.
                            if version.parse::<u64>().is_ok_and(|number| {
                                newest.as_ref().is_none_or(|newest: &String| {
                                    number > newest.parse::<u64>().unwrap_or(0)
                                })
                            }) {
                                newest = Some(version.to_owned());
                            }
                        }
                    }
                    match page.next_marker() {
                        Some(next) if !next.is_empty() => {
                            marker = Some(next.to_owned());
                        }
                        _ => break,
                    }
                }
                newest.with_context(|| {
                    format!("`{}` has no published version to point at", self.function)
                })
            }
        }
    }

    /// Every schedule name this module manages: the manifest's plus every grant the policies
    /// — the alias's and the legacy unqualified function's — still carry.
    async fn managed_names(
        &self,
        lambda: &aws_sdk_lambda::Client,
        qualified: &str,
    ) -> Result<std::collections::BTreeSet<String>> {
        let mut names: std::collections::BTreeSet<String> = self
            .schedules
            .iter()
            .map(|entry| entry.name.as_str().to_owned())
            .collect();
        for owner in [qualified, self.function.as_str()] {
            if let Some(policy) = self.policy(lambda, owner).await? {
                if let Some(statements) = policy["Statement"].as_array() {
                    for entry in statements {
                        if let Some(name) = entry["Sid"]
                            .as_str()
                            .and_then(|sid| sid.strip_prefix("skyzen-schedule-"))
                        {
                            names.insert(name.to_owned());
                        }
                    }
                }
            }
        }
        Ok(names)
    }

    /// Move every managed rule's unqualified target onto the `skyzen` alias.
    ///
    /// Rules an earlier deploy made fire the unqualified function, so the code swap alone
    /// would run new code on stale configuration. Publishing the version the function was
    /// running and pointing the alias at it moves every rule behind the atomic repoint —
    /// and is itself idempotent: a second run finds the targets already moved.
    async fn migrate_to_alias(
        &self,
        events: &aws_sdk_eventbridge::Client,
        lambda: &aws_sdk_lambda::Client,
    ) -> Result<()> {
        let arn = match lambda
            .get_function_configuration()
            .function_name(&self.function)
            .send()
            .await
        {
            Ok(configuration) => configuration
                .function_arn()
                .with_context(|| format!("`{}` reported no ARN", self.function))?
                .to_owned(),
            // A first deployment has no function and no rules to move — nothing to migrate.
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_lambda::operation::get_function_configuration::GetFunctionConfigurationError::is_resource_not_found_exception,
                ) =>
            {
                return Ok(())
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not read Lambda function `{}`", self.function)
                });
            }
        };
        let qualified = format!("{}:{ALIAS}", self.function);
        let alias_arn = format!("{arn}:{ALIAS}");

        // Only rules still carrying an unqualified target need moving — when none do, the
        // alias stays exactly where it is. Repointing it anyway would resurrect the version
        // a previous failed deployment deliberately rolled back.
        let mut pending = Vec::new();
        for name in self.managed_names(lambda, &qualified).await? {
            let rule = self.rule_name(&name);
            let listed = match events.list_targets_by_rule().rule(&rule).send().await {
                Ok(listed) => listed,
                Err(error)
                    if error.as_service_error().is_some_and(
                        aws_sdk_eventbridge::operation::list_targets_by_rule::ListTargetsByRuleError::is_resource_not_found_exception,
                    ) =>
                {
                    continue
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to list targets of `{rule}`"));
                }
            };
            // A targetless rule has nothing to move — `PutTargets` with an empty set is
            // rejected, and ordinary wiring adopts it. A rule carrying a foreign target is
            // shared with something else — retargeting its unqualified entry would mutate
            // wiring the ownership check refuses to touch.
            if listed.targets().is_empty() {
                continue;
            }
            if listed.targets().iter().all(|target| target.arn() == arn) {
                pending.push((rule, listed.targets().to_vec(), name));
            } else if listed.targets().iter().any(|target| target.arn() == arn) {
                // A rule sharing its targets with a foreign resource cannot be moved — and
                // cannot be left either: `$LATEST` swaps under it the moment deploy runs.
                // Stop here, before the code changes.
                anyhow::bail!(
                    "EventBridge rule `{rule}` shares targets with another resource — remove \
                     the foreign target before deploying"
                );
            }
        }
        if pending.is_empty() {
            return Ok(());
        }

        // The alias must exist before anything can target it — but only creation is ours:
        // an alias that already exists keeps the version its last deploy pointed it at.
        if self.alias_snapshot(lambda).await?.is_none() {
            let version = self.publish_or_reuse(lambda).await?;
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

        // arn:{partition}:lambda:{region}:{account}:function:{name}
        let parts: Vec<&str> = arn.split(':').collect();
        let (partition, region, account) = parts
            .get(1)
            .zip(parts.get(3))
            .zip(parts.get(4))
            .map(|((partition, region), account)| (*partition, *region, *account))
            .with_context(|| {
                format!("`{arn}` is not a Lambda function ARN EventBridge rules can be built from")
            })?;

        let mut migrated: Vec<(String, Vec<Target>)> = Vec::new();
        for (rule, previous, name) in pending {
            // The invoke grant precedes the retarget: a rule moved before its permission
            // would sit unable to invoke until reconciliation catches up — or forever, if
            // the deploy fails between.
            let rule_arn = format!("arn:{partition}:events:{region}:{account}:rule/{rule}");
            let statement = format!("skyzen-schedule-{name}");
            if !self
                .has_statement(lambda, &qualified, &statement, &rule_arn)
                .await
                .with_context(|| format!("failed to read the policy of `{qualified}`"))?
            {
                lambda
                    .add_permission()
                    .function_name(&qualified)
                    .action(INVOKE_ACTION)
                    .principal(EVENTS_PRINCIPAL)
                    .source_arn(&rule_arn)
                    .statement_id(&statement)
                    .send()
                    .await
                    .with_context(|| format!("failed to let `{rule}` invoke `{qualified}`"))?;
            }
            let targets: Vec<Target> = previous
                .iter()
                .map(|target| {
                    let mut moved = target.clone();
                    if moved.arn() == arn {
                        moved.arn = alias_arn.clone();
                    }
                    moved
                })
                .collect();
            let put = events
                .put_targets()
                .rule(&rule)
                .set_targets(Some(targets))
                .send()
                .await
                .with_context(|| format!("failed to retarget `{rule}` at `{ALIAS}`"))?;
            // A partially failed retarget puts every moved rule's previous targets back —
            // not just this one's — or the migration leaves earlier rules pointing at the
            // alias while later ones still fire $LATEST.
            if !put.failed_entries().is_empty() {
                for (rule, previous) in migrated
                    .iter()
                    .chain(std::iter::once(&(rule.clone(), previous.clone())))
                {
                    let _ = events
                        .put_targets()
                        .rule(rule)
                        .set_targets(Some(previous.clone()))
                        .send()
                        .await;
                }
                anyhow::bail!(
                    "EventBridge refused to retarget `{rule}`: {:?}",
                    put.failed_entries()
                        .first()
                        .map(aws_sdk_eventbridge::types::PutTargetsResultEntry::error_message)
                );
            }
            migrated.push((rule.clone(), previous));
            output::ok(format!(
                "retargeted EventBridge rule `{rule}` at the `{ALIAS}` alias"
            ));
        }
        Ok(())
    }

    /// The version the `skyzen` alias points at now, or `None` when it does not exist.
    async fn alias_snapshot(&self, lambda: &aws_sdk_lambda::Client) -> Result<Option<String>> {
        match lambda
            .get_alias()
            .function_name(&self.function)
            .name(ALIAS)
            .send()
            .await
        {
            Ok(alias) => Ok(alias.function_version().map(str::to_owned)),
            Err(error)
                if error.as_service_error().is_some_and(
                    aws_sdk_lambda::operation::get_alias::GetAliasError::is_resource_not_found_exception,
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error).with_context(|| {
                format!("failed to read the `{ALIAS}` alias of `{}`", self.function)
            }),
        }
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
        alias_arn: &str,
        snapshot: &mut Vec<RuleSnapshot>,
    ) -> Result<()> {
        const STATEMENT_PREFIX: &str = "skyzen-schedule-";

        // Grants live on the alias now, but an earlier deploy put them on the unqualified
        // function — a schedule removed while upgrading still carries that statement, and
        // its rule fires just the same. Each stale name keeps the owner it was found on:
        // rollback restores the grant where it actually lived.
        let owners: [(String, String); 2] = [
            (qualified.to_owned(), qualified.to_owned()),
            (self.function.clone(), self.function.clone()),
        ];
        let declared: std::collections::BTreeSet<&str> = self
            .schedules
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        let mut stale: Vec<(String, String, bool)> = Vec::new();
        for (resource, owner) in &owners {
            if let Some(policy) = self.policy(lambda, resource).await? {
                if let Some(statements) = policy["Statement"].as_array() {
                    for entry in statements {
                        if let Some(name) = entry["Sid"]
                            .as_str()
                            .and_then(|sid| sid.strip_prefix(STATEMENT_PREFIX))
                        {
                            // `true` — the grant existed here, so retirement removed it and
                            // rollback re-adds it on the same resource.
                            if !declared.contains(name)
                                && !stale.iter().any(|(stale, _, _)| stale == name)
                            {
                                stale.push((name.to_owned(), owner.clone(), true));
                            }
                        }
                    }
                }
            }
        }

        // The policies tell most of the story — but a rule whose grant is already gone still
        // fires its target. Names prefixed `skyzen-{function}--` are ours by construction,
        // so a prefix scan finds the survivors the grants alone would miss.
        for name in self.managed_rule_names(events).await? {
            // `false` — a rule found by name alone carries no grant, and nothing gets
            // re-added for a permission that never was.
            if !declared.contains(name.as_str())
                && !stale.iter().any(|(stale, _, _)| stale == &name)
            {
                stale.push((name, qualified.to_owned(), false));
            }
        }

        for (name, owner, had_grant) in stale {
            let rule = self.rule_name(&name);
            // The delete is one-way — the rule's own record is what a failed deploy restores.
            let mut taken = self.rule_snapshot(events, &rule).await?;
            // Only a grant that existed comes back — a rule found by name without one is
            // recreated with none, never given permission it did not have.
            taken.statement = had_grant.then(|| format!("{STATEMENT_PREFIX}{name}"));
            taken.owner = Some(owner);
            // Retiring mutates the rule the moment it deletes it — the snapshot restores.
            taken.mutated = true;
            snapshot.push(taken);
            self.retire_rule(events, &[arn, alias_arn], &rule).await?;
            for owner in [qualified, self.function.as_str()] {
                match lambda
                    .remove_permission()
                    .function_name(owner)
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
                                "failed to drop the invoke permission of `{rule}` on `{owner}`"
                            )
                        });
                    }
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
        arns: &[&str],
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
        if let Some(target) = listed
            .targets()
            .iter()
            .find(|target| !arns.contains(&target.arn()))
        {
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

    /// Every rule name this module owns: the `skyzen-{function}--` prefix is ours by
    /// construction, whatever the policies say.
    async fn managed_rule_names(
        &self,
        events: &aws_sdk_eventbridge::Client,
    ) -> Result<Vec<String>> {
        let prefix = format!("skyzen-{}--", self.function);
        let mut names = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut list = events.list_rules().name_prefix(&prefix);
            if let Some(next) = &token {
                list = list.next_token(next);
            }
            let page = list
                .send()
                .await
                .with_context(|| "failed to list this function's EventBridge rules")?;
            for rule in page.rules() {
                if let Some(name) = rule
                    .name()
                    .and_then(|name| name.strip_prefix(prefix.as_str()))
                {
                    names.push(name.to_owned());
                }
            }
            match page.next_token() {
                Some(next) if !next.is_empty() => token = Some(next.to_owned()),
                _ => break,
            }
        }
        Ok(names)
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
        arns: &[&str],
        rule: &str,
        mutated: &mut bool,
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
        if let Some(target) = listed
            .targets()
            .iter()
            .find(|target| !arns.contains(&target.arn()))
        {
            anyhow::bail!(
                "EventBridge rule `{rule}` already exists and targets `{}`, not Lambda function \
                 `{}`; rename the schedule or remove the foreign rule",
                target.arn(),
                self.function
            );
        }

        // Everything after this line changes the rule — ownership is verified, and the
        // snapshot restores from here on. `mutated` is recorded before the first mutation,
        // or a failed prune would leave a rolled-back rule missing targets.
        *mutated = true;
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
        mutated: &mut bool,
        granted: &mut bool,
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

        // A rule an earlier deploy made still points at the unqualified function — it is
        // ours, and wiring retargets it at the alias.
        let legacy = arn.strip_suffix(&format!(":{ALIAS}")).unwrap_or(arn);
        self.own_the_rule(events, &[arn, legacy], &rule, mutated)
            .await?;

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
        // the second case would leave the rule unable to invoke. The caller records whether
        // the grant is new — a failed deploy removes what it added.
        let rule_arn = format!("arn:{partition}:events:{region}:{account}:rule/{rule}");
        let statement = format!("skyzen-schedule-{}", entry.name);
        let needs_grant = !self
            .has_statement(lambda, qualified, &statement, &rule_arn)
            .await
            .with_context(|| {
                format!(
                    "failed to read the invoke policy of Lambda function `{}`",
                    self.function
                )
            })?;
        if needs_grant {
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
            // Recorded the moment the grant lands — a later wiring failure rolls the
            // permission back too, not just the rule.
            *granted = true;
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

        // The rule stays disabled through the whole transition — `reconcile` switches every
        // schedule on at once only after the alias points at the new version, so no event
        // can catch the old handler behind the new cron.
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
