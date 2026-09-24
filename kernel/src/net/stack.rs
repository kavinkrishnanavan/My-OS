//! TCP/IP stack wiring: DHCP for an address, then sockets for DNS + TCP.
//!
//! Everything in here is driven from `poll_task`, an async task that
//! yields (returns `Poll::Pending`, registering a waker) whenever there's
//! nothing to do, instead of looping on "is the NIC ready yet?". The
//! executor only resumes it when the RTL8139 IRQ handler actually wakes
//! it up, or the periodic timer tick does (smoltcp still needs occasional
//! wakeups for its own retransmit/DHCP-lease timers, not just RX).

use crate::net::{rtl8139, Rtl8139Device};
use crate::{serial_println, task::time};
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::{dhcpv4, dns, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use spin::Mutex;

/// Every task waiting on network activity registers its `Waker` here;
/// `wake()` drains and wakes all of them. A single-slot `AtomicWaker`
/// (the more obvious choice) is a *single-waiter* primitive — with more
/// than one task registering (both `net_poll_loop` and the HTTP fetch
/// task do), each `register` silently discards whatever waker was there
/// before, permanently orphaning the other task. That's a deadlock this
/// codebase actually hit: DHCP completed (net_poll_loop got its one
/// chance to run) but DNS resolution then hung forever, because by the
/// time the HTTP task's own wait loop registered, `net_poll_loop`'s
/// waker had already been silently dropped and `stack::poll()` was never
/// called again.
struct WakerSet(Mutex<Vec<Waker>>);

impl WakerSet {
    const fn new() -> Self {
        WakerSet(Mutex::new(Vec::new()))
    }

    /// Must run with interrupts disabled: `wake()` below locks the same
    /// mutex from IRQ context (NIC RX, PIT tick), and if that fires
    /// while this function holds the lock, it's the same single-CPU
    /// self-deadlock class as `interrupts::unmask_irq` — see its doc
    /// comment for the full explanation.
    fn register(&self, waker: &Waker) {
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut wakers = self.0.lock();
            if !wakers.iter().any(|w| w.will_wake(waker)) {
                wakers.push(waker.clone());
            }
        });
    }

    fn wake(&self) {
        let ready: Vec<Waker> = core::mem::take(&mut *self.0.lock());
        for waker in ready {
            waker.wake();
        }
    }
}

/// Woken by the RTL8139 IRQ handler (a packet arrived) and by the PIT
/// tick (so smoltcp's own retransmit/DHCP timers still fire even with no
/// traffic). Anything waiting on the network stack registers here
/// instead of polling in a loop — see `NetTick` below.
static NET_WAKER: WakerSet = WakerSet::new();

/// Awaiting this suspends the current task until the network stack has
/// something new to look at, without ever spinning: the executor is free
/// to `hlt` in the meantime, and only this task gets resumed, by
/// `NET_WAKER.wake()`, when there's actually a reason to.
pub struct NetTick {
    registered: bool,
}

pub fn net_tick() -> NetTick {
    NetTick { registered: false }
}

/// Called from interrupt context (NIC RX/TX, PIT tick) to resume every
/// task currently waiting on `net_tick()`.
pub fn wake_net_waiters() {
    NET_WAKER.wake();
}

impl Future for NetTick {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<()> {
        if self.registered {
            Poll::Ready(())
        } else {
            self.registered = true;
            NET_WAKER.register(cx.waker());
            Poll::Pending
        }
    }
}

pub struct NetStack {
    pub device: Rtl8139Device,
    pub iface: Interface,
    pub sockets: SocketSet<'static>,
    pub dhcp_handle: SocketHandle,
    pub dns_handle: SocketHandle,
}

pub static STACK: Mutex<Option<NetStack>> = Mutex::new(None);

fn now() -> Instant {
    Instant::from_millis(time::uptime_ms() as i64)
}

pub fn init() {
    let mac = rtl8139::mac();
    let mut device = Rtl8139Device;

    let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let mut iface = Interface::new(config, &mut device, now());
    iface.update_ip_addrs(|addrs| {
        // Placeholder until DHCP hands us a real lease; smoltcp requires
        // at least an empty set to exist before that.
        addrs.clear();
    });

    let mut sockets = SocketSet::new(vec![]);

    let dhcp_socket = dhcpv4::Socket::new();
    let dhcp_handle = sockets.add(dhcp_socket);

    let dns_socket = dns::Socket::new(&[], vec![]);
    let dns_handle = sockets.add(dns_socket);

    *STACK.lock() = Some(NetStack {
        device,
        iface,
        sockets,
        dhcp_handle,
        dns_handle,
    });

    serial_println!("net: interface up, waiting for DHCP lease");
}

/// Runs one iteration of the interface state machine. Returns whether
/// anything changed (new lease, socket data ready, ...) so callers can
/// decide whether to re-check their own socket state this tick.
pub fn poll() -> bool {
    let mut guard = STACK.lock();
    let stack = guard.as_mut().expect("net::init not called");
    let timestamp = now();

    let socket_state_changed = stack
        .iface
        .poll(timestamp, &mut stack.device, &mut stack.sockets);

    let dhcp_event = stack
        .sockets
        .get_mut::<dhcpv4::Socket>(stack.dhcp_handle)
        .poll();
    if let Some(dhcpv4::Event::Configured(config)) = dhcp_event {
        serial_println!(
            "net: DHCP lease {} via gateway {:?}",
            config.address,
            config.router
        );
        stack.iface.update_ip_addrs(|addrs| {
            addrs.clear();
            addrs.push(IpCidr::Ipv4(config.address)).ok();
        });
        if let Some(router) = config.router {
            stack.iface.routes_mut().add_default_ipv4_route(router).ok();
        }
        serial_println!("net: DHCP-provided DNS servers: {:?}", config.dns_servers);
        if !config.dns_servers.is_empty() {
            let servers: vec::Vec<IpAddress> =
                config.dns_servers.iter().map(|a| IpAddress::Ipv4(*a)).collect();
            stack
                .sockets
                .get_mut::<dns::Socket>(stack.dns_handle)
                .update_servers(&servers);
        }
    }

    socket_state_changed
}

pub fn has_ip() -> bool {
    let mut guard = STACK.lock();
    let stack = guard.as_mut().expect("net::init not called");
    stack.iface.ipv4_addr().is_some()
}

pub fn with_stack<R>(f: impl FnOnce(&mut NetStack) -> R) -> R {
    let mut guard = STACK.lock();
    let stack = guard.as_mut().expect("net::init not called");
    f(stack)
}

pub fn new_tcp_socket() -> tcp::Socket<'static> {
    let rx_buffer = tcp::SocketBuffer::new(vec![0u8; 4096]);
    let tx_buffer = tcp::SocketBuffer::new(vec![0u8; 4096]);
    tcp::Socket::new(rx_buffer, tx_buffer)
}
