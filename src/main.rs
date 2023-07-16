/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart JMAP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::time::Duration;

use directory::config::ConfigDirectory;
use jmap::{api::JmapSessionManager, services::IPC_CHANNEL_BUFFER, JMAP};
use smtp::core::{SmtpSessionManager, SMTP};
use tokio::sync::mpsc;
use utils::{
    config::{Config, ServerProtocol},
    enable_tracing, wait_for_shutdown, UnwrapFailure,
};

#[cfg(not(target_env = "msvc"))]
use jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let config = Config::init();
    let servers = config.parse_servers().failed("Invalid configuration");
    let directory = config.parse_directory().failed("Invalid configuration");

    // Bind ports and drop privileges
    servers.bind(&config);

    // Enable tracing
    let _tracer = enable_tracing(
        &config,
        &format!(
            "Starting Stalwart JMAP Server v{}...",
            env!("CARGO_PKG_VERSION"),
        ),
    )
    .failed("Failed to enable tracing");

    // Init servers
    let (delivery_tx, delivery_rx) = mpsc::channel(IPC_CHANNEL_BUFFER);
    let smtp = SMTP::init(&config, &servers, &directory, delivery_tx)
        .await
        .failed("Invalid configuration file");
    let jmap = JMAP::init(&config, &directory, delivery_rx, smtp.clone())
        .await
        .failed("Invalid configuration file");

    // Spawn servers
    let shutdown_tx = servers.spawn(|server, shutdown_rx| {
        match &server.protocol {
            ServerProtocol::Smtp | ServerProtocol::Lmtp => {
                server.spawn(SmtpSessionManager::new(smtp.clone()), shutdown_rx)
            }
            ServerProtocol::Http => {
                tracing::debug!("Ignoring HTTP server listener, using JMAP port instead.");
            }
            ServerProtocol::Jmap => {
                server.spawn(JmapSessionManager::new(jmap.clone()), shutdown_rx)
            }
            ServerProtocol::Imap => {
                tracing::debug!(
                    "Ignoring IMAP server listener, not supported by JMAP-only release."
                );
            }
            ServerProtocol::ManageSieve => {
                tracing::debug!(
                    "Ignoring ManageSieve server listener, not supported by JMAP-only release."
                );
            }
        };
    });

    // Wait for shutdown signal
    wait_for_shutdown(&format!(
        "Shutting down Stalwart JMAP Server v{}...",
        env!("CARGO_PKG_VERSION")
    ))
    .await;

    // Stop services
    let _ = shutdown_tx.send(true);

    // Wait for services to finish
    tokio::time::sleep(Duration::from_secs(1)).await;

    Ok(())
}
