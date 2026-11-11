//! Bringing this host to `[native.linux]`.
//!
//! The host is read with the doctor's own readers ([`Host::probe`], and the steering module's
//! receive-queue interrupts), the manifest's stated values are compared against it, and each one
//! the host does not have yet becomes one [`Operation`]. Applying them and planning again finds
//! nothing, which is what makes a deployment repeatable and what the closing [`Verify`] step
//! checks. A facility the host lacks or does not report fails the plan naming it: nothing is
//! skipped and nothing is guessed.
//!
//! Every write goes through the kernel interface the doctor reads: procfs for the sysctls and the
//! interrupt affinities, sysfs for the interface attributes, ethtool netlink for the features and
//! ring parameters, and TCP's own upper-layer-protocol lookup for the `tls` module.

use std::{fmt, fs, path::PathBuf};

use anyhow::{Context, Result};
use ethtool::{EthtoolRingParsed, EthtoolTcpDataSplit};
use skyzen_manifest::{KernelTlsState, NativeLinuxNic, NativeLinuxSection, RxIrqPlacement};

use super::{
    nic_directory, serving_threads, DataSplit, Host, KernelTls, Nic, Offload, Probe, Sysctl,
    Unknown, BUSY_POLL, BUSY_READ, GRO_FLUSH_TIMEOUT, NAPI_DEFER_HARD_IRQS, TLS_HW_RX_OFFLOAD,
    TLS_HW_TX_OFFLOAD,
};
use crate::{
    output,
    providers::{Step, Task},
};

/// The steps that bring this host to `section`: one per setting it does not have yet, then
/// [`Verify`]. None when the host already has every setting.
///
/// # Errors
///
/// The section asks for a facility the host lacks or does not report.
pub async fn steps(section: &NativeLinuxSection) -> Result<Vec<Step>> {
    let plan = Plan::read(section).await?;
    for setting in &plan.already {
        output::ok(format!("native: {setting}"));
    }
    if plan.operations.is_empty() {
        output::ok("native: this host already has every [native.linux] setting");
        return Ok(Vec::new());
    }
    let mut steps: Vec<Step> = plan
        .operations
        .into_iter()
        .map(|operation| Step::Task(Box::new(operation)))
        .collect();
    steps.push(Step::Task(Box::new(Verify {
        section: section.clone(),
    })));
    Ok(steps)
}

/// What a section asks of this host.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    /// The settings the host does not have yet.
    operations: Vec<Operation>,
    /// The settings it already has, for the report.
    already: Vec<String>,
}

/// The receive-queue interrupts the section's `rx_irqs` places, and where they may go.
#[derive(Debug)]
struct Interrupts {
    /// The CPUs the serving threads are pinned to.
    serving: Probe<Vec<usize>>,
    /// Each physical interface's receive-queue interrupts.
    rx: Probe<Vec<NicIrqs>>,
}

/// One interface's receive-queue interrupts.
#[derive(Debug)]
struct NicIrqs {
    nic: String,
    irqs: Vec<RxIrq>,
}

/// One receive-queue interrupt, and the CPUs it may be delivered to.
#[derive(Debug)]
struct RxIrq {
    irq: u32,
    affinity: Probe<Vec<usize>>,
}

impl Interrupts {
    fn read() -> Self {
        let serving = serving_threads().and_then(|threads| {
            skyzen_core::steering::serving_cpus(threads)
                .map_err(|error| Unknown(format!("the serving CPUs cannot be read: {error}")))
        });
        let rx = skyzen_core::steering::rx_interrupts()
            .map(|nics| {
                nics.into_iter()
                    .map(|(nic, irqs)| NicIrqs {
                        nic,
                        irqs: irqs
                            .into_iter()
                            .map(|irq| RxIrq {
                                irq,
                                affinity: skyzen_core::steering::irq_affinity(irq).map_err(
                                    |error| {
                                        Unknown(format!(
                                            "the affinity of IRQ {irq} cannot be read: {error}"
                                        ))
                                    },
                                ),
                            })
                            .collect(),
                    })
                    .collect()
            })
            .map_err(|error| {
                Unknown(format!(
                    "the receive-queue interrupts cannot be read: {error}"
                ))
            });
        Self { serving, rx }
    }
}

impl Plan {
    /// Read this host and compare it against `section`.
    async fn read(section: &NativeLinuxSection) -> Result<Self> {
        let host = Host::probe().await;
        let places_irqs = section.nic.values().any(|nic| nic.rx_irqs.is_some());
        let interrupts = places_irqs.then(Interrupts::read);
        Self::of(section, &host, interrupts.as_ref())
    }

    /// The plan for `section` on a host read as `host` and `interrupts`.
    fn of(
        section: &NativeLinuxSection,
        host: &Host,
        interrupts: Option<&Interrupts>,
    ) -> Result<Self> {
        let mut plan = Self::default();
        if section.kernel_tls == Some(KernelTlsState::Loaded) {
            match facility("kernel_tls", &host.kernel_tls)? {
                KernelTls::Loaded => plan.already.push("the `tls` module is loaded".to_owned()),
                KernelTls::Loadable => plan.operations.push(Operation::LoadTls),
                KernelTls::Absent => anyhow::bail!(
                    "[native.linux] kernel_tls = \"loaded\": this kernel has no `tls` module"
                ),
            }
        }
        for (sysctl, wanted, current) in [
            (BUSY_POLL, section.busy_poll, &host.busy_poll.poll_micros),
            (BUSY_READ, section.busy_read, &host.busy_poll.read_micros),
        ] {
            let Some(wanted) = wanted else { continue };
            let current = *facility(sysctl.name, current)?;
            if current == wanted {
                plan.already.push(format!("{} is {wanted}", sysctl.name));
            } else {
                plan.operations.push(Operation::Sysctl {
                    sysctl,
                    current,
                    wanted,
                });
            }
        }
        if section.nic.is_empty() {
            return Ok(plan);
        }
        let nics = facility("the network interfaces", &host.nics)?;
        for (name, wanted) in &section.nic {
            let Some(nic) = nics.iter().find(|nic| nic.name == *name) else {
                let names: Vec<&str> = nics.iter().map(|nic| nic.name.as_str()).collect();
                anyhow::bail!(
                    "[native.linux.nic.{name}]: this host has no physical interface `{name}` \
                     (it has: {})",
                    if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names.join(", ")
                    }
                );
            };
            plan.nic(nic, wanted, interrupts)?;
        }
        Ok(plan)
    }

    /// The operations one interface's section asks for.
    fn nic(
        &mut self,
        nic: &Nic,
        wanted: &NativeLinuxNic,
        interrupts: Option<&Interrupts>,
    ) -> Result<()> {
        let name = &nic.name;
        for (feature, wanted, state) in [
            (TLS_HW_TX_OFFLOAD, wanted.tls_hw_tx_offload, &nic.tls_tx),
            (TLS_HW_RX_OFFLOAD, wanted.tls_hw_rx_offload, &nic.tls_rx),
        ] {
            let Some(enable) = wanted else { continue };
            let state = *facility(&format!("{name} {feature}"), state)?;
            match (enable, state) {
                (true, Offload::Active) | (false, Offload::Inactive | Offload::Unsupported) => {
                    self.already
                        .push(format!("{name}: {feature} is {}", on_off(enable)));
                }
                (true, Offload::Unsupported) => anyhow::bail!(
                    "[native.linux.nic.{name}] {}: {name} cannot turn {feature} on",
                    feature.replace('-', "_")
                ),
                (true, Offload::Inactive) | (false, Offload::Active) => {
                    self.operations.push(Operation::Feature {
                        nic: name.clone(),
                        feature,
                        enable,
                    });
                }
            }
        }
        if let Some(enable) = wanted.tcp_data_split {
            let current = *facility(&format!("{name} tcp-data-split"), &nic.data_split)?;
            if (current == DataSplit::Enabled) == enable {
                self.already
                    .push(format!("{name}: tcp-data-split is {}", on_off(enable)));
            } else {
                self.operations.push(Operation::DataSplit {
                    nic: name.clone(),
                    enable,
                });
            }
        }
        for (attribute, wanted, current) in [
            (
                NAPI_DEFER_HARD_IRQS,
                wanted.napi_defer_hard_irqs.map(u64::from),
                nic.napi_defer_hard_irqs.clone().map(u64::from),
            ),
            (
                GRO_FLUSH_TIMEOUT,
                wanted.gro_flush_timeout,
                nic.gro_flush_timeout.clone(),
            ),
        ] {
            let Some(wanted) = wanted else { continue };
            let current = *facility(&format!("{name} {attribute}"), &current)?;
            if current == wanted {
                self.already
                    .push(format!("{name}: {attribute} is {wanted}"));
            } else {
                self.operations.push(Operation::NicAttribute {
                    nic: name.clone(),
                    attribute,
                    current,
                    wanted,
                });
            }
        }
        if wanted.rx_irqs == Some(RxIrqPlacement::Serving) {
            let interrupts = interrupts.context("the receive-queue interrupts were not read")?;
            let serving = facility("the serving CPUs", &interrupts.serving)?;
            let nics = facility("the receive-queue interrupts", &interrupts.rx)?;
            let irqs = nics
                .iter()
                .find(|nic| nic.nic == *name)
                .map(|nic| nic.irqs.as_slice())
                .unwrap_or_default();
            if irqs.is_empty() {
                anyhow::bail!(
                    "[native.linux.nic.{name}] rx_irqs = \"serving\": {name} has no \
                     receive-queue interrupt in /proc/interrupts"
                );
            }
            for (RxIrq { irq, affinity }, &cpu) in irqs.iter().zip(serving.iter().cycle()) {
                let irq = *irq;
                let affinity = facility(&format!("{name} IRQ {irq}"), affinity)?;
                if affinity.as_slice() == [cpu] {
                    self.already
                        .push(format!("{name}: receive-queue IRQ {irq} goes to CPU {cpu}"));
                } else {
                    self.operations.push(Operation::IrqAffinity {
                        nic: name.clone(),
                        irq,
                        current: affinity.clone(),
                        cpu,
                    });
                }
            }
        }
        Ok(())
    }
}

/// A facility's state, or the failure naming what could not be read.
fn facility<'a, T>(what: &str, probe: &'a Probe<T>) -> Result<&'a T> {
    probe.as_ref().map_err(|why| {
        anyhow::anyhow!("[native.linux] {what}: this host does not tell its state: {why}")
    })
}

const fn on_off(enable: bool) -> &'static str {
    if enable {
        "on"
    } else {
        "off"
    }
}

/// One change that brings a setting to its stated value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Operation {
    /// Register the `tls` upper-layer protocol with TCP.
    LoadTls,
    /// Write a sysctl.
    Sysctl {
        sysctl: Sysctl,
        current: u32,
        wanted: u32,
    },
    /// Write an interface attribute under `/sys/class/net/<nic>/`.
    NicAttribute {
        nic: String,
        attribute: &'static str,
        current: u64,
        wanted: u64,
    },
    /// Turn an interface feature on or off.
    Feature {
        nic: String,
        feature: &'static str,
        enable: bool,
    },
    /// Turn an interface's header/data split on or off.
    DataSplit { nic: String, enable: bool },
    /// Deliver one receive-queue interrupt to one CPU.
    IrqAffinity {
        nic: String,
        irq: u32,
        current: Vec<usize>,
        cpu: usize,
    },
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadTls => f.write_str(
                "native: load the kernel `tls` module: setsockopt(TCP_ULP, \"tls\"), which has \
                 the kernel request module `tcp-ulp-tls`",
            ),
            Self::Sysctl {
                sysctl,
                current,
                wanted,
            } => write!(
                f,
                "native: set {} to {wanted} (now {current}): write {}",
                sysctl.name, sysctl.path
            ),
            Self::NicAttribute {
                nic,
                attribute,
                current,
                wanted,
            } => write!(
                f,
                "native: set {nic} {attribute} to {wanted} (now {current}): write {}",
                nic_directory(nic).join(attribute).display()
            ),
            Self::Feature {
                nic,
                feature,
                enable,
            } => write!(
                f,
                "native: turn {nic} {feature} {}: ethtool netlink ETHTOOL_MSG_FEATURES_SET",
                on_off(*enable)
            ),
            Self::DataSplit { nic, enable } => write!(
                f,
                "native: turn {nic} tcp-data-split {}: ethtool netlink ETHTOOL_MSG_RINGS_SET",
                on_off(*enable)
            ),
            Self::IrqAffinity {
                nic,
                irq,
                current,
                cpu,
            } => write!(
                f,
                "native: deliver {nic} receive-queue IRQ {irq} to CPU {cpu} (now {current:?}): \
                 write {}",
                irq_affinity_path(*irq).display()
            ),
        }
    }
}

fn irq_affinity_path(irq: u32) -> PathBuf {
    std::path::Path::new("/proc/irq")
        .join(irq.to_string())
        .join(skyzen_core::steering::SMP_AFFINITY_LIST)
}

impl Task for Operation {
    fn describe(&self) -> String {
        self.to_string()
    }

    fn run(&self) -> Result<()> {
        match self {
            Self::LoadTls => load_tls(),
            Self::Sysctl { sysctl, wanted, .. } => write(sysctl.path.as_ref(), wanted),
            Self::NicAttribute {
                nic,
                attribute,
                wanted,
                ..
            } => write(&nic_directory(nic).join(attribute), wanted),
            Self::Feature {
                nic,
                feature,
                enable,
            } => crate::runtime::block_on(set_feature(nic, feature, *enable))?,
            Self::DataSplit { nic, enable } => {
                crate::runtime::block_on(set_data_split(nic, *enable))?
            }
            Self::IrqAffinity { irq, cpu, .. } => write(&irq_affinity_path(*irq), cpu),
        }
    }
}

/// Write `value` to a procfs or sysfs file.
fn write(path: &std::path::Path, value: &impl fmt::Display) -> Result<()> {
    fs::write(path, value.to_string()).with_context(|| format!("cannot write {}", path.display()))
}

/// Have the kernel load the `tls` module the way the runtime's first kernel-TLS socket would.
///
/// Setting the `TCP_ULP` option of a TCP socket to `tls` makes the kernel look the protocol up,
/// and, when it is missing and the caller has `CAP_NET_ADMIN`, request the module aliased
/// `tcp-ulp-tls` (`tcp_ulp_find_autoload`), with its dependencies, through the kernel's own module
/// loader. The socket is never connected, so the option itself is refused with `ENOTCONN` once
/// the module is there.
fn load_tls() -> Result<()> {
    use nix::{
        errno::Errno,
        sys::socket::{setsockopt, socket, sockopt::TcpUlp, AddressFamily, SockFlag, SockType},
    };

    let socket = socket(
        AddressFamily::Inet,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .context("cannot open a TCP socket")?;
    match setsockopt(&socket, TcpUlp::default(), b"tls") {
        Ok(()) | Err(Errno::ENOTCONN) => Ok(()),
        Err(Errno::ENOENT) => anyhow::bail!(
            "the kernel could not load the `tls` module (setsockopt TCP_ULP answered ENOENT)"
        ),
        Err(error) => Err(error).context("setsockopt(TCP_ULP, \"tls\") failed for another reason"),
    }
}

async fn set_feature(nic: &str, feature: &str, enable: bool) -> Result<()> {
    let (connection, mut handle, _) =
        ethtool::new_connection().context("cannot open a generic netlink socket")?;
    let driver = tokio::spawn(connection);
    let unapplied = handle
        .feature()
        .set(nic)
        .feature(feature, enable)
        .execute()
        .await;
    driver.abort();
    let unapplied = unapplied.with_context(|| {
        format!(
            "ethtool netlink refused turning {nic} {feature} {}",
            on_off(enable)
        )
    })?;
    if !unapplied.is_empty() {
        anyhow::bail!(
            "{nic}'s driver did not turn {feature} {}, though it lists it as changeable",
            on_off(enable)
        );
    }
    Ok(())
}

async fn set_data_split(nic: &str, enable: bool) -> Result<()> {
    let (connection, mut handle, _) =
        ethtool::new_connection().context("cannot open a generic netlink socket")?;
    let driver = tokio::spawn(connection);
    let mut config = EthtoolRingParsed::default();
    config.iface_name = Some(nic.to_owned());
    config.tcp_data_split = Some(if enable {
        EthtoolTcpDataSplit::Enabled
    } else {
        EthtoolTcpDataSplit::Disabled
    });
    let applied = handle.apply_ring(config).await;
    driver.abort();
    applied.with_context(|| {
        format!(
            "ethtool netlink refused turning {nic} tcp-data-split {}",
            on_off(enable)
        )
    })
}

/// The closing step: read the host again and require that nothing is left to change, then report
/// what connection steering decides on it.
#[derive(Debug)]
struct Verify {
    section: NativeLinuxSection,
}

impl Task for Verify {
    fn describe(&self) -> String {
        "native: read the host again: every [native.linux] setting must now hold".to_owned()
    }

    fn run(&self) -> Result<()> {
        let plan = crate::runtime::block_on(Plan::read(&self.section))??;
        if !plan.operations.is_empty() {
            let left: Vec<String> = plan.operations.iter().map(ToString::to_string).collect();
            anyhow::bail!(
                "the host did not take every [native.linux] setting; still to do:\n{}",
                left.join("\n")
            );
        }
        output::ok("native: this host has every [native.linux] setting");
        if self.section.nic.values().any(|nic| nic.rx_irqs.is_some()) {
            match super::steering() {
                Ok(steering) => steering.report(),
                Err(why) => output::unknown(format!("native: connection steering: {why}")),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use skyzen_core::steering::{Placement, RxLayout};

    use super::*;
    use crate::host::linux::{BusyPoll, LockedMemory, Steering};

    fn host(nics: Vec<Nic>) -> Host {
        Host {
            kernel: Ok("6.18.0".to_owned()),
            kernel_tls: Ok(KernelTls::Loadable),
            nics: Ok(nics),
            busy_poll: BusyPoll {
                poll_micros: Ok(0),
                read_micros: Ok(50),
                napi_registration: Ok(true),
            },
            steering: Ok(Steering {
                threads: 2,
                placement: Ok(Placement {
                    cpus: vec![0, 1],
                    layout: RxLayout::default(),
                }),
            }),
            locked_memory: LockedMemory {
                limit: Err(Unknown("not read".to_owned())),
                ipc_lock: Ok(true),
            },
        }
    }

    fn nic(name: &str) -> Nic {
        Nic {
            name: name.to_owned(),
            tls_tx: Ok(Offload::Inactive),
            tls_rx: Ok(Offload::Unsupported),
            data_split: Err(Unknown("the driver does not report it".to_owned())),
            napi_defer_hard_irqs: Ok(0),
            gro_flush_timeout: Ok(20_000),
        }
    }

    fn section(nic: Option<(&str, NativeLinuxNic)>) -> NativeLinuxSection {
        NativeLinuxSection {
            kernel_tls: Some(KernelTlsState::Loaded),
            busy_poll: Some(50),
            busy_read: Some(50),
            nic: nic
                .map(|(name, nic)| (name.to_owned(), nic))
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn only_what_differs_becomes_an_operation() {
        let plan = Plan::of(
            &section(Some((
                "eth0",
                NativeLinuxNic {
                    tls_hw_tx_offload: Some(true),
                    tls_hw_rx_offload: Some(false),
                    napi_defer_hard_irqs: Some(2),
                    gro_flush_timeout: Some(20_000),
                    ..NativeLinuxNic::default()
                },
            ))),
            &host(vec![nic("eth0")]),
            None,
        )
        .expect("a plan");

        assert_eq!(
            plan.operations,
            [
                Operation::LoadTls,
                Operation::Sysctl {
                    sysctl: BUSY_POLL,
                    current: 0,
                    wanted: 50
                },
                Operation::Feature {
                    nic: "eth0".to_owned(),
                    feature: TLS_HW_TX_OFFLOAD,
                    enable: true
                },
                Operation::NicAttribute {
                    nic: "eth0".to_owned(),
                    attribute: NAPI_DEFER_HARD_IRQS,
                    current: 0,
                    wanted: 2
                },
            ]
        );
        assert_eq!(
            plan.already,
            [
                "net.core.busy_read is 50",
                "eth0: tls-hw-rx-offload is off",
                "eth0: gro_flush_timeout is 20000"
            ]
        );
        assert_eq!(
            plan.operations[1].to_string(),
            "native: set net.core.busy_poll to 50 (now 0): write /proc/sys/net/core/busy_poll"
        );
    }

    #[test]
    fn a_facility_the_host_lacks_or_does_not_report_fails_naming_it() {
        let fails = |section: NativeLinuxSection, host: &Host| {
            Plan::of(&section, host, None)
                .expect_err("refused")
                .to_string()
        };
        let mut absent = host(Vec::new());
        absent.kernel_tls = Ok(KernelTls::Absent);
        assert!(fails(section(None), &absent).contains("no `tls` module"));

        let unsupported = NativeLinuxNic {
            tls_hw_rx_offload: Some(true),
            ..NativeLinuxNic::default()
        };
        let error = fails(
            section(Some(("eth0", unsupported))),
            &host(vec![nic("eth0")]),
        );
        assert!(
            error.contains("cannot turn tls-hw-rx-offload on"),
            "{error}"
        );

        let split = NativeLinuxNic {
            tcp_data_split: Some(true),
            ..NativeLinuxNic::default()
        };
        let error = fails(section(Some(("eth0", split))), &host(vec![nic("eth0")]));
        assert!(error.contains("eth0 tcp-data-split"), "{error}");
        assert!(error.contains("does not report"), "{error}");

        let error = fails(
            section(Some(("eth9", NativeLinuxNic::default()))),
            &host(vec![nic("eth0")]),
        );
        assert!(
            error.contains("no physical interface `eth9` (it has: eth0)"),
            "{error}"
        );
    }

    #[test]
    fn receive_queue_interrupts_go_to_the_serving_cpus_in_turn() {
        let interrupts = Interrupts {
            serving: Ok(vec![0, 1]),
            rx: Ok(vec![NicIrqs {
                nic: "eth0".to_owned(),
                irqs: [(24, vec![0]), (25, vec![0, 1]), (26, vec![1])]
                    .into_iter()
                    .map(|(irq, cpus)| RxIrq {
                        irq,
                        affinity: Ok(cpus),
                    })
                    .collect(),
            }]),
        };
        let mut section = section(None);
        section.kernel_tls = None;
        section.busy_poll = None;
        section.busy_read = None;
        section.nic.insert(
            "eth0".to_owned(),
            NativeLinuxNic {
                rx_irqs: Some(RxIrqPlacement::Serving),
                ..NativeLinuxNic::default()
            },
        );

        let plan = Plan::of(&section, &host(vec![nic("eth0")]), Some(&interrupts)).expect("plan");

        assert_eq!(
            plan.operations,
            [
                Operation::IrqAffinity {
                    nic: "eth0".to_owned(),
                    irq: 25,
                    current: vec![0, 1],
                    cpu: 1
                },
                Operation::IrqAffinity {
                    nic: "eth0".to_owned(),
                    irq: 26,
                    current: vec![1],
                    cpu: 0
                },
            ]
        );
        assert_eq!(plan.already, ["eth0: receive-queue IRQ 24 goes to CPU 0"]);
    }
}
