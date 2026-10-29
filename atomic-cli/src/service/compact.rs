use super::{pb, services_maintenance, Backend, Service};
use crate::error::CliResult;
use libatomic::atomic::maintenance_service_server::MaintenanceService as _;
use tonic::{Request, Status};

impl Service {
    pub fn compact_database(
        &self,
        request: pb::CompactDatabaseRequest,
    ) -> CliResult<pb::CompactDatabaseResponse> {
        self.call(move |backend| async move {
            let mut request = Request::new(request);
            request
                .metadata_mut()
                .insert("x-atomic-contract-version", "2".parse().unwrap());
            match backend {
                Backend::Local(state) => services_maintenance::MaintenanceImpl { state }
                    .compact_database(request)
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut daemon =
                        pb::daemon_service_client::DaemonServiceClient::new(channel.clone());
                    let capabilities = daemon
                        .get_capabilities(pb::GetCapabilitiesRequest {})
                        .await?
                        .into_inner();
                    if capabilities.protocol_version != 2 {
                        return Err(Status::failed_precondition(
                            "the running Reactor uses an incompatible service contract; update/restart it and retry",
                        ));
                    }
                    let supported = capabilities.methods.iter().any(|method| {
                        method.service == "MaintenanceService" && method.method == "CompactDatabase"
                    });
                    if !supported {
                        return Err(Status::failed_precondition(
                            "the running Reactor does not support database compaction; update/restart it and retry",
                        ));
                    }
                    require_cleanup_capability(request.get_ref(), &capabilities.features)?;
                    pb::maintenance_service_client::MaintenanceServiceClient::new(channel)
                        .compact_database(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

fn require_cleanup_capability(
    request: &pb::CompactDatabaseRequest,
    features: &[String],
) -> Result<(), Status> {
    if request.cleanup
        && !features
            .iter()
            .any(|feature| feature == "compact_cleanup_v1")
    {
        return Err(Status::failed_precondition(
            "the running Reactor lacks journal-aware compaction; update/restart it or use --database-only (dry-run/full must not be ignored)",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_peer_cannot_silently_turn_preview_or_full_into_an_ordinary_write() {
        for (dry_run, full) in [(true, false), (true, true), (false, true), (false, false)] {
            let request = pb::CompactDatabaseRequest {
                cleanup: true,
                dry_run,
                full,
                ..Default::default()
            };
            assert_eq!(
                require_cleanup_capability(&request, &[])
                    .unwrap_err()
                    .code(),
                tonic::Code::FailedPrecondition
            );
            assert!(
                require_cleanup_capability(&request, &["compact_cleanup_v1".to_string()]).is_ok()
            );
        }
        assert!(require_cleanup_capability(&Default::default(), &[]).is_ok());
    }
}
