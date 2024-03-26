/*
Copyright 2023 The Kubernetes Authors.

SPDX-License-Identifier: (GPL-2.0-only OR BSD-2-Clause)
*/

use core::mem;

use aya_ebpf::{
    bindings::TC_ACT_OK,
    helpers::{bpf_csum_diff, bpf_redirect_neigh},
    programs::TcContext,
};
use aya_log_ebpf::{debug, info};
use network_types::{eth::EthHdr, ip::Ipv4Hdr, tcp::TcpHdr};

use crate::{
    utils::{csum_fold_helper, ptr_at, update_tcp_conns},
    BACKENDS, GATEWAY_INDEXES, LB_CONNECTIONS,
};
use common::{
    Backend, BackendKey, ClientKey, LoadBalancerMapping, TCPState, BACKENDS_ARRAY_CAPACITY,
};

pub fn handle_tcp_ingress(ctx: TcContext) -> Result<i32, i64> {
    let ip_hdr: *mut Ipv4Hdr = unsafe { ptr_at(&ctx, EthHdr::LEN)? };

    let tcp_header_offset = EthHdr::LEN + Ipv4Hdr::LEN;

    let tcp_hdr: *mut TcpHdr = unsafe { ptr_at(&ctx, tcp_header_offset) }?;

    let original_daddr = unsafe { (*ip_hdr).dst_addr };

    // The source identifier
    let client_key = ClientKey {
        ip: u32::from_be(unsafe { (*ip_hdr).src_addr }),
        port: (u16::from_be(unsafe { (*tcp_hdr).source })) as u32,
    };
    // The backend that is responsible for handling this TCP connection.
    let mut backend: Backend;
    // The Gateway that the TCP connections is forwarded from.
    let backend_key: BackendKey;
    // Flag to check whether this is a new connection.
    let mut new_conn = false;
    // The state of this TCP connection.
    let mut tcp_state = Some(TCPState::default());

    // Try to find the backend previously used for this connection. If not found, it means that
    // this is a new connection, so assign it the next backend in line.
    if let Some(val) = unsafe { LB_CONNECTIONS.get(&client_key) } {
        backend = val.backend;
        backend_key = val.backend_key;
        tcp_state = val.tcp_state;
    } else {
        new_conn = true;

        backend_key = BackendKey {
            ip: u32::from_be(original_daddr),
            port: (u16::from_be(unsafe { (*tcp_hdr).dest })) as u32,
        };
        let backend_list = unsafe { BACKENDS.get(&backend_key) }.ok_or(TC_ACT_OK)?;
        let backend_index = unsafe { GATEWAY_INDEXES.get(&backend_key) }.ok_or(TC_ACT_OK)?;

        debug!(&ctx, "Destination backend index: {}", *backend_index);
        debug!(&ctx, "Backends length: {}", backend_list.backends_len);

        // this check asserts that we don't use a "zero-value" Backend
        if backend_list.backends_len <= *backend_index {
            return Ok(TC_ACT_OK);
        }
        // the bpf verifier is aware of variables that are used as an index for
        // an array and requires that we check the array boundaries against
        // the index to ensure our access is in-bounds.
        if *backend_index as usize >= BACKENDS_ARRAY_CAPACITY {
            return Ok(TC_ACT_OK);
        }

        backend = backend_list.backends[0];
        if let Some(val) = backend_list.backends.get(*backend_index as usize) {
            backend = *val;
        } else {
            debug!(
                &ctx,
                "Failed to find backend in backends_list at index {}, falling back to 0th index; backends_len: {} ",
                *backend_index,
                backend_list.backends_len
            )
        }

        // move the index to the next backend in our list
        let mut next = *backend_index + 1;
        if next >= backend_list.backends_len {
            next = 0;
        }
        unsafe {
            GATEWAY_INDEXES.insert(&backend_key, &next, 0_u64)?;
        }
    }

    info!(
        &ctx,
        "Received a TCP packet destined for svc ip: {:i} at Port: {} ",
        u32::from_be(original_daddr),
        u16::from_be(unsafe { (*tcp_hdr).dest })
    );

    // DNAT the ip address
    unsafe {
        (*ip_hdr).dst_addr = backend.daddr.to_be();
    }
    // DNAT the port
    unsafe { (*tcp_hdr).dest = (backend.dport as u16).to_be() };

    if (ctx.data() + EthHdr::LEN + Ipv4Hdr::LEN) > ctx.data_end() {
        info!(&ctx, "Iphdr is out of bounds");
        return Ok(TC_ACT_OK);
    }

    // Calculate l3 cksum
    // TODO(astoycos) use l3_cksum_replace instead
    unsafe { (*ip_hdr).check = 0 };
    let full_cksum = unsafe {
        bpf_csum_diff(
            mem::MaybeUninit::zeroed().assume_init(),
            0,
            ip_hdr as *mut u32,
            Ipv4Hdr::LEN as u32,
            0,
        )
    } as u64;
    unsafe { (*ip_hdr).check = csum_fold_helper(full_cksum) };
    // FIXME
    unsafe { (*tcp_hdr).check = 0 };

    let action = unsafe {
        bpf_redirect_neigh(
            backend.ifindex as u32,
            mem::MaybeUninit::zeroed().assume_init(),
            0,
            0,
        )
    };

    let mut lb_mapping = LoadBalancerMapping {
        backend,
        backend_key,
        tcp_state,
    };

    // If the connection is new, then record it in our map for future tracking.
    if new_conn {
        unsafe {
            LB_CONNECTIONS.insert(&client_key, &lb_mapping, 0_u64)?;
        }

        // since this is a new connection, there is nothing else to do, so exit early
        info!(&ctx, "redirect action: {}", action);
        return Ok(action as i32);
    }

    let tcp_hdr_ref = unsafe { tcp_hdr.as_ref().ok_or(TC_ACT_OK)? };

    // If the packet has the RST flag set, it means the connection is being terminated, so remove it
    // from our map.
    if tcp_hdr_ref.rst() == 1 {
        unsafe {
            LB_CONNECTIONS.remove(&client_key)?;
        }
    }

    update_tcp_conns(tcp_hdr_ref, &client_key, &mut lb_mapping)?;

    info!(&ctx, "redirect action: {}", action);
    Ok(action as i32)
}
