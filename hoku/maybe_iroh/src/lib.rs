// Copyright 2024 Textile
// Copyright 2022-2024 Protocol Labs
// SPDX-License-Identifier: Apache-2.0, MIT

use std::net::ToSocketAddrs;

use anyhow::anyhow;
use iroh::client::Iroh;

#[derive(Clone, Debug)]
pub struct MaybeIroh {
    addr: Option<String>,
    client: Option<Iroh>,
}

impl MaybeIroh {
    pub fn from_addr(addr: String) -> MaybeIroh {
        Self {
            addr: Some(addr),
            client: None,
        }
    }

    pub fn maybe_addr(addr: Option<String>) -> MaybeIroh {
        Self { addr, client: None }
    }

    pub async fn client(&mut self) -> anyhow::Result<Iroh> {
        if let Some(c) = self.client.clone() {
            return Ok(c);
        }
        if let Some(addr) = self.addr.clone() {
            let addr = addr.to_socket_addrs()?.next().ok_or(anyhow!(
                "failed to convert iroh node address to a socket address"
            ))?;
            match Iroh::connect_addr(addr).await {
                Ok(client) => {
                    self.client = Some(client.clone());
                    Ok(client)
                }
                Err(e) => Err(e),
            }
        } else {
            Err(anyhow!("iroh node address is not configured"))
        }
    }
}
