//! Kernel attribution for the connecting half of an IPv4 loopback TCP pair.
//! Evidence is a rate-budget key only, never authentication or operator authority.
//! macOS follows [XNU's socket record ABI](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/netinet/in_pcblist.c).
//! Only IPv4 is needed by the listener; unsupported families remain unknown.
use std::net::SocketAddr;

/// Match both endpoints while the accepted connection keeps the tuple occupied.
/// Missing, malformed, oversized or inaccessible kernel evidence remains unknown.
/// Call only on a bounded blocking worker, before admitting HTTP headers.
pub(crate) fn lookup(peer: SocketAddr, local: SocketAddr) -> Option<u32> {
    let (SocketAddr::V4(peer), SocketAddr::V4(local)) = (peer, local) else {
        return None;
    };
    if !peer.ip().is_loopback() || !local.ip().is_loopback() {
        return None;
    }
    kernel(peer, local)
}

#[cfg(target_os = "macos")]
fn kernel(peer: std::net::SocketAddrV4, local: std::net::SocketAddrV4) -> Option<u32> {
    // XNU exports aligned length/kind records; cap the snapshot instead of retrying.
    let mut bytes = vec![0u8; 4 * 1024 * 1024];
    let mut len = bytes.len();
    // SAFETY: the name is terminated and sysctl receives the live buffer's capacity.
    if unsafe {
        libc::sysctlbyname(
            c"net.inet.tcp.pcblist_n".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    mac_records(bytes.get(..len)?, peer, local)
}

#[cfg(any(target_os = "macos", test))]
fn mac_records(
    bytes: &[u8],
    peer: std::net::SocketAddrV4,
    local: std::net::SocketAddrV4,
) -> Option<u32> {
    // Packed XNU xinpcb_n and xsocket_n prefixes (in_pcb.h and socketvar.h).
    // Only a socket record paired to the matching PCB can supply its so_uid.
    let number = |bytes: &[u8], offset| {
        Some(u32::from_ne_bytes(
            bytes.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let mut offset = number(bytes, 0)? as usize;
    if offset != 24 {
        return None;
    }
    let mut pcb = None;
    while offset < bytes.len() {
        let record = bytes.get(offset..)?;
        let len = number(record, 0)? as usize;
        if len == 24 {
            break;
        }
        if len < 8 {
            return None;
        }
        let record = record.get(..len)?;
        match number(record, 4)? {
            0x10 => {
                pcb = None;
                if record.get(44)? & 1 != 0
                    && record.get(16..18)? == local.port().to_be_bytes()
                    && record.get(18..20)? == peer.port().to_be_bytes()
                    && record.get(60..64)? == local.ip().octets()
                    && record.get(76..80)? == peer.ip().octets()
                {
                    pcb = Some(record.get(8..16)?);
                }
            }
            1 if pcb.is_some() => {
                if record.get(28..36)? == pcb? {
                    return number(record, 64);
                }
                return None;
            }
            _ => {}
        }
        offset = offset.checked_add(len.checked_add(7)? & !7)?;
    }
    None
}

#[cfg(target_os = "linux")]
fn kernel(peer: std::net::SocketAddrV4, local: std::net::SocketAddrV4) -> Option<u32> {
    use std::io::Read;
    let mut bytes = String::new();
    std::fs::File::open("/proc/net/tcp")
        .ok()?
        .take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut bytes)
        .ok()?;
    if bytes.len() > 4 * 1024 * 1024 {
        return None;
    }
    linux_records(&bytes, peer, local)
}

#[cfg(any(target_os = "linux", test))]
fn linux_records(
    bytes: &str,
    peer: std::net::SocketAddrV4,
    local: std::net::SocketAddrV4,
) -> Option<u32> {
    let endpoint = |addr: std::net::SocketAddrV4| {
        format!(
            "{:08X}:{:04X}",
            u32::from_ne_bytes(addr.ip().octets()),
            addr.port()
        )
    };
    let (peer, local) = (endpoint(peer), endpoint(local));
    bytes.lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        (fields.get(1)? == &peer && fields.get(2)? == &local && fields.get(3)? == &"01")
            .then(|| fields.get(7)?.parse().ok())
            .flatten()
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn kernel(_: std::net::SocketAddrV4, _: std::net::SocketAddrV4) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests;
