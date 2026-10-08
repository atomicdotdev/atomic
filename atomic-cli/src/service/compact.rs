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
                    pb::maintenance_service_client::MaintenanceServiceClient::new(channel)
                        .compact_database(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}
