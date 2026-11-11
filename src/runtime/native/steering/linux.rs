//! The kernel half of steering: CPU pinning, `SO_INCOMING_CPU` and the reuseport program. Whether
//! to steer is read from the machine by `skyzen_core::steering`, which `skyzen doctor` shares.

use std::{io, mem, net::TcpListener, os::fd::AsRawFd};

use skyzen_core::steering::{Placement, Unavailable};
use tracing::info;

use super::{Pins, Steering, SteeringError};
use crate::runtime::native::ListenAddr;

/// `SKF_AD_OFF + SKF_AD_CPU`: the ancillary load of the CPU the filter runs on, which for a
/// reuseport group is the CPU that took the SYN.
const LOAD_CPU: u32 = (-0x1000_i32 + 36).cast_unsigned();

/// Pin the calling thread to `cpu`.
pub(in crate::runtime::native) fn pin_current_thread(cpu: usize) -> io::Result<()> {
    // SAFETY: an all-zero `cpu_set_t` is the empty set, and `cpu` is below `CPU_SETSIZE`
    // because the mask it came from held it.
    let result = unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &raw const set)
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

impl Pins {
    /// Thread `i` takes the `i`-th CPU of the process's affinity mask.
    ///
    /// # Errors
    ///
    /// The mask cannot be read or holds fewer than `threads` CPUs.
    #[cfg(test)]
    pub(in crate::runtime::native) fn from_affinity(threads: usize) -> io::Result<Self> {
        Ok(Self {
            cpus: skyzen_core::steering::serving_cpus(threads)?,
        })
    }

    /// The CPU thread `index` serves, when steering.
    pub(in crate::runtime::native) fn cpu(&self, index: usize) -> Option<usize> {
        self.cpus.get(index).copied()
    }

    /// Whether any thread is pinned.
    pub(in crate::runtime::native) const fn is_steering(&self) -> bool {
        !self.cpus.is_empty()
    }

    /// Pin the calling thread, which is serving thread `index`, to its CPU.
    pub(in crate::runtime::native) fn pin(&self, index: usize) -> Result<(), SteeringError> {
        self.cpu(index).map_or(Ok(()), |cpu| {
            pin_current_thread(cpu).map_err(|error| SteeringError::Pin { cpu, error })
        })
    }

    /// Attach the program that sends a connection to the listener of the CPU it arrived on.
    ///
    /// `listeners` are the group's members in the order they joined it, which is the index the
    /// program answers with; each must already carry its `SO_INCOMING_CPU`.
    ///
    /// # Errors
    ///
    /// The kernel refuses the program.
    pub(in crate::runtime::native) fn attach(&self, listeners: &[TcpListener]) -> io::Result<()> {
        let Some(first) = listeners.first().filter(|_| self.is_steering()) else {
            return Ok(());
        };
        let mut program = reuseport_program(&self.cpus)?;
        let filter = libc::sock_fprog {
            len: u16::try_from(program.len()).map_err(io::Error::other)?,
            filter: program.as_mut_ptr(),
        };
        // SAFETY: `filter` points at `len` instructions that outlive the call, and the kernel
        // copies them.
        let result = unsafe {
            libc::setsockopt(
                first.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ATTACH_REUSEPORT_CBPF,
                (&raw const filter).cast(),
                libc::socklen_t::try_from(mem::size_of::<libc::sock_fprog>())
                    .map_err(io::Error::other)?,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// The classic BPF program: load the CPU, and for each served CPU in turn return its listener's
/// index. A CPU no listener serves returns an index past the group, which makes the kernel fall
/// back to its hash.
///
/// # Errors
///
/// A CPU or listener index does not fit the 32-bit operand of a BPF instruction.
fn reuseport_program(cpus: &[usize]) -> io::Result<Vec<libc::sock_filter>> {
    // The BPF opcodes are fixed values below 0x100.
    #[allow(clippy::cast_possible_truncation)]
    const LD_W_ABS: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    #[allow(clippy::cast_possible_truncation)]
    const JEQ_K: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    #[allow(clippy::cast_possible_truncation)]
    const RET_K: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

    let operand = |value: usize| u32::try_from(value).map_err(io::Error::other);
    let insn = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let mut program = Vec::with_capacity(2 + 2 * cpus.len());
    program.push(insn(LD_W_ABS, 0, 0, LOAD_CPU));
    for (index, &cpu) in cpus.iter().enumerate() {
        program.push(insn(JEQ_K, 0, 1, operand(cpu)?));
        program.push(insn(RET_K, 0, 0, operand(index)?));
    }
    program.push(insn(RET_K, 0, 0, u32::MAX));
    Ok(program)
}

/// Report to the kernel that `listener` serves `cpu`.
///
/// # Errors
///
/// The kernel refuses the option.
pub(in crate::runtime::native) fn set_incoming_cpu(
    listener: &socket2::Socket,
    cpu: usize,
) -> io::Result<()> {
    listener.set_cpu_affinity(cpu)
}

pub(super) fn decide(
    setting: Steering,
    threads: usize,
    addr: &ListenAddr,
) -> Result<Pins, SteeringError> {
    if setting == Steering::Off {
        return Ok(Pins::default());
    }
    if !matches!(addr, ListenAddr::Tcp(_)) {
        return if setting == Steering::On {
            Err(SteeringError::NotTcp)
        } else {
            info!("Connection steering is off: the listener is a unix socket");
            Ok(Pins::default())
        };
    }
    let placement = match Placement::read(threads) {
        Ok(placement) => placement,
        Err(Unavailable::Affinity(error)) => return Err(SteeringError::Affinity(error)),
        Err(Unavailable::Layout(error)) if setting == Steering::On => {
            return Err(SteeringError::Layout(error));
        }
        Err(Unavailable::Layout(error)) => {
            info!("Connection steering is off: the NIC queue layout cannot be read: {error}");
            return Ok(Pins::default());
        }
    };
    if placement.steers() {
        info!(
            cpus = ?placement.cpus,
            "Connection steering is on: every serving CPU takes a NIC receive queue"
        );
        return Ok(Pins {
            cpus: placement.cpus,
        });
    }
    let unserved = placement.unserved();
    if setting == Steering::On {
        return Err(SteeringError::Unserved(unserved));
    }
    info!(
        cpus = ?unserved,
        "Connection steering is off: these serving CPUs take no NIC receive queue's interrupt \
         (force it with --steering on)"
    );
    Ok(Pins::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_maps_each_served_cpu_to_its_listener() {
        let program = reuseport_program(&[2, 5]).unwrap();
        assert_eq!(program.len(), 6);
        assert_eq!((program[1].k, program[2].k), (2, 0));
        assert_eq!((program[3].k, program[4].k), (5, 1));
        assert_eq!(program[5].k, u32::MAX);
    }
}
