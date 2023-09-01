use std::net::SocketAddr;
use std::str::FromStr;
use tauri::{AppHandle, Manager, Runtime, State};
use tracing::{debug, info, trace, warn};

use ockam_api::address::{extract_address_value, get_free_address};
use ockam_api::cli_state::{CliState, StateDirTrait};
use ockam_api::cloud::project::Project;
use ockam_api::cloud::share::InvitationWithAccess;
use ockam_api::cloud::share::{InvitationListKind, ListInvitations};
use ockam_api::nodes::models::portal::InletStatus;

use crate::app::{AppState, NODE_NAME};
use crate::cli::cli_bin;

use super::{events::REFRESHED_REMOTE_SERVICES, state::SyncState};

pub async fn refresh_remote_services<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    debug!("refreshing remote services");
    let state: State<'_, AppState> = app.state();
    if !state.is_enrolled().await.unwrap_or(false) {
        debug!("not enrolled, skipping invitations refresh");
        return Ok(());
    }
    let node_manager_worker = state.node_manager_worker().await;
    let invitations = node_manager_worker
        .list_shares(
            &state.context(),
            ListInvitations {
                kind: InvitationListKind::All,
            },
            &state.controller_address(),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;
    trace!(?invitations);
    {
        let invitation_state: State<'_, SyncState> = app.state();
        let mut writer = invitation_state.write().await;
        writer.replace_by(invitations);
    }
    refresh_inlets(&app).await.map_err(|e| e.to_string())?;
    app.trigger_global(REFRESHED_REMOTE_SERVICES, None);
    Ok(())
}

async fn refresh_inlets<R: Runtime>(app: &AppHandle<R>) -> crate::Result<()> {
    debug!("Refreshing inlets");
    let invitations_state: State<'_, SyncState> = app.state();
    let mut writer = invitations_state.write().await;
    if writer.services.invitations.is_empty() {
        return Ok(());
    }
    let app_state: State<'_, AppState> = app.state();
    let cli_state = app_state.state().await;
    let cli_bin = cli_bin()?;
    let mut inlets_socket_addrs = vec![];
    for invitation in &writer.services.invitations {
        match InletDataFromInvitation::new(&cli_state, invitation) {
            Ok(i) => match i {
                Some(i) => {
                    let mut inlet_is_running = false;
                    debug!(node = %i.local_node_name, "Checking node status");
                    if let Ok(node) = cli_state.nodes.get(&i.local_node_name) {
                        if node.is_running() {
                            debug!(node = %i.local_node_name, "Node already running");
                            debug!(node = %i.local_node_name, "Checking TCP inlet status");
                            if let Ok(cmd) = duct::cmd!(
                                &cli_bin,
                                "tcp-inlet",
                                "show",
                                "--quiet",
                                &i.service_name,
                                "--at",
                                &i.local_node_name,
                                "--output",
                                "json"
                            )
                            .stdout_capture()
                            .run()
                            {
                                let inlet: InletStatus = serde_json::from_slice(&cmd.stdout)?;
                                let inlet_socket_addr = SocketAddr::from_str(&inlet.bind_addr)?;
                                inlet_is_running = true;
                                debug!(
                                    at = ?inlet.bind_addr,
                                    alias = inlet.alias,
                                    "TCP inlet running"
                                );
                                inlets_socket_addrs
                                    .push((invitation.invitation.id.clone(), inlet_socket_addr));
                            }
                        }
                    }
                    if inlet_is_running {
                        continue;
                    }
                    debug!(node = %i.local_node_name, "Deleting node");
                    let _ = duct::cmd!(
                        &cli_bin,
                        "node",
                        "delete",
                        "--quiet",
                        "--yes",
                        &i.local_node_name
                    )
                    .run();
                    match create_inlet(&i).await {
                        Ok(inlet_socket_addr) => {
                            inlets_socket_addrs
                                .push((invitation.invitation.id.clone(), inlet_socket_addr));
                        }
                        Err(err) => {
                            warn!(%err, node = %i.local_node_name, "Failed to create tcp-inlet for accepted invitation");
                        }
                    }
                }
                None => {
                    warn!("Invalid invitation data");
                }
            },
            Err(err) => {
                warn!(%err, "Failed to parse invitation data");
            }
        }
    }
    for (invitation_id, inlet_socket_addr) in inlets_socket_addrs {
        writer
            .services
            .inlets
            .insert(invitation_id, inlet_socket_addr);
    }
    info!("Inlets refreshed");
    Ok(())
}

/// Create the tcp-inlet for the accepted invitation
/// Returns the inlet SocketAddr
async fn create_inlet(inlet_data: &InletDataFromInvitation) -> crate::Result<SocketAddr> {
    debug!(service_name = ?inlet_data.service_name, "Creating TCP inlet for accepted invitation");
    let InletDataFromInvitation {
        local_node_name,
        service_name,
        service_route,
        enrollment_ticket_hex,
    } = inlet_data;
    let from = get_free_address()?;
    let from_str = from.to_string();
    if let Some(enrollment_ticket_hex) = enrollment_ticket_hex {
        let _ = duct::cmd!(
            cli_bin()?,
            "--no-input",
            "project",
            "enroll",
            "--new-trust-context-name",
            &local_node_name,
            &enrollment_ticket_hex,
        )
        .run();
        debug!(node = %local_node_name, "Node enrolled using enrollment ticket");
    }
    duct::cmd!(
        cli_bin()?,
        "--no-input",
        "node",
        "create",
        &local_node_name,
        "--trust-context",
        &local_node_name
    )
    .run()?;
    debug!(node = %local_node_name, "Node created");
    duct::cmd!(
        cli_bin()?,
        "--no-input",
        "tcp-inlet",
        "create",
        "--at",
        &local_node_name,
        "--from",
        &from_str,
        "--to",
        &service_route,
        "--alias",
        &service_name,
    )
    .run()?;
    info!(
        from = from_str,
        to = service_route,
        "Created tcp-inlet for accepted invitation"
    );
    Ok(from)
}

#[derive(Debug)]
struct InletDataFromInvitation {
    pub local_node_name: String,
    pub service_name: String,
    pub service_route: String,
    pub enrollment_ticket_hex: Option<String>,
}

impl InletDataFromInvitation {
    pub fn new(
        cli_state: &CliState,
        invitation: &InvitationWithAccess,
    ) -> crate::Result<Option<Self>> {
        match &invitation.service_access_details {
            Some(d) => {
                let service_name = extract_address_value(&d.shared_node_route)?;
                let enrollment_ticket_hex = if invitation.invitation.is_expired()? {
                    None
                } else {
                    Some(d.enrollment_ticket.clone())
                };
                let enrollment_ticket = d.enrollment_ticket()?;
                if let Some(project) = enrollment_ticket.project() {
                    let mut project = Project::from(project.clone());

                    // Rename project so that the local user can receive multiple invitations from different
                    // projects called "default" while keeping access to its own "default" project.
                    // Te node created here is meant to only serve the tcp-inlet and only has to resolve
                    // the `/project/{id}` project to create the needed secure-channel.
                    project.name = project.id.clone();
                    let project = cli_state
                        .projects
                        .overwrite(project.name.clone(), project)?;

                    let project_id = project.id();
                    let local_node_name = format!("ockam_app_{project_id}_{service_name}");
                    let service_route = format!(
                        "/project/{project_id}/service/forward_to_{}/secure/api/service/{service_name}",
                        NODE_NAME
                    );

                    Ok(Some(Self {
                        local_node_name,
                        service_name,
                        service_route,
                        enrollment_ticket_hex,
                    }))
                } else {
                    warn!(?invitation, "No project data found in enrollment ticket");
                    Ok(None)
                }
            }
            None => {
                warn!(
                    ?invitation,
                    "No service details found in accepted invitation"
                );
                Ok(None)
            }
        }
    }
}