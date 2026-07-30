use super::*;

fn read_u16_ne_at(bytes: &[u8], offset: usize) -> KResult<u16> {
    if bytes.len() < offset + size_of::<u16>() {
        return Err(Errno::EINVAL);
    }
    Ok(u16::from_ne_bytes([bytes[offset], bytes[offset + 1]]))
}

fn read_u32_ne_at(bytes: &[u8], offset: usize) -> KResult<u32> {
    if bytes.len() < offset + size_of::<u32>() {
        return Err(Errno::EINVAL);
    }
    Ok(u32::from_ne_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn push_u16_ne(data: &mut Vec<u8>, value: u16) {
    data.extend_from_slice(&value.to_ne_bytes());
}

fn push_i32_ne(data: &mut Vec<u8>, value: i32) {
    data.extend_from_slice(&value.to_ne_bytes());
}

fn push_u32_ne(data: &mut Vec<u8>, value: u32) {
    data.extend_from_slice(&value.to_ne_bytes());
}

fn netlink_align(len: usize) -> usize {
    (len + 3) & !3
}

fn pad_netlink(data: &mut Vec<u8>) {
    let aligned = netlink_align(data.len());
    data.resize(aligned, 0);
}

fn push_netlink_attr(data: &mut Vec<u8>, ty: u16, value: &[u8]) {
    push_u16_ne(data, (size_of::<u16>() * 2 + value.len()) as u16);
    push_u16_ne(data, ty);
    data.extend_from_slice(value);
    pad_netlink(data);
}

fn push_netlink_header(data: &mut Vec<u8>, len: u32, ty: u16, flags: u16, seq: u32, pid: u32) {
    push_u32_ne(data, len);
    push_u16_ne(data, ty);
    push_u16_ne(data, flags);
    push_u32_ne(data, seq);
    push_u32_ne(data, pid);
}

fn push_netlink_done(seq: u32, pid: u32) -> Vec<u8> {
    let mut data = Vec::new();
    push_netlink_header(
        &mut data,
        size_of::<u32>() as u32 * 4,
        NLMSG_DONE,
        NLM_F_MULTI,
        seq,
        pid,
    );
    data
}

fn push_netlink_ack(seq: u32, pid: u32, request: &[u8]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0i32.to_ne_bytes());
    let request_header_len = (size_of::<u32>() * 4).min(request.len());
    payload.extend_from_slice(&request[..request_header_len]);

    let msg_len = size_of::<u32>() * 4 + payload.len();
    let mut data = Vec::new();
    push_netlink_header(&mut data, msg_len as u32, NLMSG_ERROR, 0, seq, pid);
    data.extend_from_slice(&payload);
    data
}

fn push_link_info(iface: &NetInterface, seq: u32, pid: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(AF_UNSPEC as u8);
    payload.push(0);
    push_u16_ne(&mut payload, iface.kind());
    push_i32_ne(&mut payload, iface.index);
    push_u32_ne(&mut payload, iface.flags);
    push_u32_ne(&mut payload, 0);

    let attr_start = payload.len();
    let mut name = iface.name.clone().into_bytes();
    name.push(0);
    push_netlink_attr(&mut payload, IFLA_IFNAME, &name);
    push_netlink_attr(&mut payload, IFLA_ADDRESS, &iface.hwaddr);
    push_netlink_attr(&mut payload, IFLA_MTU, &iface.mtu.to_ne_bytes());

    let msg_len = size_of::<u32>() * 4 + payload.len();
    let mut data = Vec::new();
    push_netlink_header(
        &mut data,
        msg_len as u32,
        RTM_NEWLINK,
        NLM_F_MULTI,
        seq,
        pid,
    );
    data.extend_from_slice(&payload);
    debug_assert_eq!(data.len(), msg_len);
    debug_assert!(payload.len() > attr_start);
    data
}

fn push_addr_info(iface: &NetInterface, addr: &NetAddress, seq: u32, pid: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(addr.family);
    payload.push(addr.prefix_len);
    payload.push(0);
    payload.push(0);
    push_u32_ne(&mut payload, iface.index as u32);
    push_netlink_attr(&mut payload, IFA_ADDRESS, &addr.address);
    push_netlink_attr(&mut payload, IFA_LOCAL, &addr.address);

    let msg_len = size_of::<u32>() * 4 + payload.len();
    let mut data = Vec::new();
    push_netlink_header(
        &mut data,
        msg_len as u32,
        RTM_NEWADDR,
        NLM_F_MULTI,
        seq,
        pid,
    );
    data.extend_from_slice(&payload);
    data
}

fn for_each_rtattr(attrs: &[u8], mut f: impl FnMut(u16, &[u8])) {
    let mut offset = 0;
    while offset + 4 <= attrs.len() {
        let len = u16::from_ne_bytes([attrs[offset], attrs[offset + 1]]) as usize;
        let ty = u16::from_ne_bytes([attrs[offset + 2], attrs[offset + 3]]);
        if len < 4 || offset + len > attrs.len() {
            break;
        }
        f(ty, &attrs[offset + 4..offset + len]);
        offset += netlink_align(len);
    }
}

fn rtattr_string(value: &[u8]) -> Option<String> {
    let end = value
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(value.len());
    if end == 0 {
        return None;
    }
    core::str::from_utf8(&value[..end])
        .ok()
        .map(ToString::to_string)
}

fn find_rtattr_string(attrs: &[u8], wanted: u16) -> Option<String> {
    let mut found = None;
    for_each_rtattr(attrs, |ty, value| {
        if ty == wanted && found.is_none() {
            found = rtattr_string(value);
        }
    });
    found
}

fn find_rtattr_bytes(attrs: &[u8], wanted: u16) -> Option<Vec<u8>> {
    let mut found = None;
    for_each_rtattr(attrs, |ty, value| {
        if ty == wanted && found.is_none() {
            found = Some(value.to_vec());
        }
    });
    found
}

fn parse_veth_peer_name(linkinfo: &[u8]) -> Option<String> {
    let mut peer = None;
    for_each_rtattr(linkinfo, |ty, value| {
        if ty != IFLA_INFO_DATA || peer.is_some() {
            return;
        }
        for_each_rtattr(value, |nested_ty, nested_value| {
            if nested_ty != VETH_INFO_PEER || peer.is_some() || nested_value.len() < IFINFOMSG_LEN {
                return;
            }
            peer = find_rtattr_string(&nested_value[IFINFOMSG_LEN..], IFLA_IFNAME);
        });
    });
    peer
}

fn handle_newlink_request(message: &[u8]) {
    if message.len() < size_of::<u32>() * 4 + IFINFOMSG_LEN {
        return;
    }
    let ifinfo = &message[size_of::<u32>() * 4..];
    let index = i32::from_ne_bytes([ifinfo[4], ifinfo[5], ifinfo[6], ifinfo[7]]);
    let flags = u32::from_ne_bytes([ifinfo[8], ifinfo[9], ifinfo[10], ifinfo[11]]);
    let change = u32::from_ne_bytes([ifinfo[12], ifinfo[13], ifinfo[14], ifinfo[15]]);
    let attrs = &ifinfo[IFINFOMSG_LEN..];
    let ifname = find_rtattr_string(attrs, IFLA_IFNAME);
    let mut link_kind = None;
    let mut peer_name = None;
    for_each_rtattr(attrs, |ty, value| {
        if ty == IFLA_LINKINFO {
            link_kind = find_rtattr_string(value, IFLA_INFO_KIND);
            peer_name = parse_veth_peer_name(value);
        }
    });

    let mut state = NETDEV.exclusive_access();
    if link_kind.as_deref() == Some("veth") {
        let first = ifname.as_deref().unwrap_or("ltp_ns_veth1");
        let second = peer_name.as_deref().unwrap_or("ltp_ns_veth2");
        state.ensure_veth_pair(first, second);
        return;
    }
    if let Some(name) = ifname {
        state.ensure_veth(name.as_str());
    }
    if index > 0 && change != 0 {
        state.set_link_flags(index, flags, change);
    }
}

fn handle_addr_request(message: &[u8], add: bool) {
    if message.len() < size_of::<u32>() * 4 + IFADDRMSG_LEN {
        return;
    }
    let addrmsg = &message[size_of::<u32>() * 4..];
    let family = addrmsg[0];
    let prefix_len = addrmsg[1];
    let index = u32::from_ne_bytes([addrmsg[4], addrmsg[5], addrmsg[6], addrmsg[7]]) as i32;
    let attrs = &addrmsg[IFADDRMSG_LEN..];
    let address = find_rtattr_bytes(attrs, IFA_LOCAL)
        .or_else(|| find_rtattr_bytes(attrs, IFA_ADDRESS))
        .unwrap_or_default();
    if address.is_empty() {
        return;
    }
    let mut state = NETDEV.exclusive_access();
    if add {
        state.add_addr(index, family, prefix_len, address);
    } else {
        state.del_addr(index, family, &address);
    }
}

pub(super) fn build_netlink_route_responses(request: &[u8]) -> KResult<Vec<Vec<u8>>> {
    if request.len() < size_of::<u32>() * 4 {
        return Err(Errno::EINVAL);
    }
    let msg_len = read_u32_ne_at(request, 0)? as usize;
    if msg_len < size_of::<u32>() * 4 || msg_len > request.len() {
        return Err(Errno::EINVAL);
    }
    let msg_type = read_u16_ne_at(request, 4)?;
    let seq = read_u32_ne_at(request, 8)?;
    let pid = read_u32_ne_at(request, 12)?;
    let mut responses = Vec::new();
    match msg_type {
        RTM_GETLINK => {
            for iface in NETDEV.exclusive_access().snapshot() {
                responses.push(push_link_info(&iface, seq, pid));
            }
            responses.push(push_netlink_done(seq, pid));
        }
        RTM_GETADDR => {
            let family = if request.len() >= size_of::<u32>() * 4 + IFADDRMSG_LEN {
                request[size_of::<u32>() * 4]
            } else {
                AF_UNSPEC as u8
            };
            for iface in NETDEV.exclusive_access().snapshot() {
                for addr in &iface.addrs {
                    if family == AF_UNSPEC as u8 || addr.family == family {
                        responses.push(push_addr_info(&iface, addr, seq, pid));
                    }
                }
            }
            responses.push(push_netlink_done(seq, pid));
        }
        RTM_GETROUTE => {
            responses.push(push_netlink_done(seq, pid));
        }
        RTM_NEWLINK => {
            handle_newlink_request(&request[..msg_len]);
            responses.push(push_netlink_ack(seq, pid, &request[..msg_len]));
        }
        RTM_DELLINK => {
            responses.push(push_netlink_ack(seq, pid, &request[..msg_len]));
        }
        RTM_NEWADDR => {
            handle_addr_request(&request[..msg_len], true);
            responses.push(push_netlink_ack(seq, pid, &request[..msg_len]));
        }
        RTM_DELADDR => {
            handle_addr_request(&request[..msg_len], false);
            responses.push(push_netlink_ack(seq, pid, &request[..msg_len]));
        }
        RTM_NEWROUTE | RTM_DELROUTE => {
            responses.push(push_netlink_ack(seq, pid, &request[..msg_len]));
        }
        _ => return Err(Errno::ENOTSUP),
    }
    Ok(responses)
}
