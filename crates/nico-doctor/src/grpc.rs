use std::collections::HashMap;
use async_trait::async_trait;
use anyhow::Result;
use futures::stream;
use prost::Message;
use tonic::transport::Channel;
use tonic_reflection::pb::v1alpha::{
    server_reflection_client::ServerReflectionClient,
    server_reflection_request::MessageRequest,
    server_reflection_response::MessageResponse,
    ServerReflectionRequest,
};

pub struct GrpcServiceInfo {
    #[allow(dead_code)]
    pub name: String,
    pub method_count: usize,
}

#[allow(dead_code)]
pub enum GrpcInspectResult {
    Reachable { services: Vec<GrpcServiceInfo> },
    Unreachable,
}

#[async_trait]
pub trait GrpcInspector: Send + Sync {
    async fn inspect(&self, addr: &str) -> Result<GrpcInspectResult>;
}

pub struct TonicGrpcInspector;

#[async_trait]
impl GrpcInspector for TonicGrpcInspector {
    async fn inspect(&self, addr: &str) -> Result<GrpcInspectResult> {
        let channel = match Channel::from_shared(addr.to_string())
            .map_err(|e| anyhow::anyhow!("invalid gRPC address: {e}"))?
            .connect()
            .await
        {
            Ok(ch) => ch,
            Err(_) => return Ok(GrpcInspectResult::Unreachable),
        };

        let mut client = ServerReflectionClient::new(channel);

        // Step 1: list services
        let list_req = ServerReflectionRequest {
            host: String::new(),
            message_request: Some(MessageRequest::ListServices(String::new())),
        };
        let service_names: Vec<String> = match client
            .server_reflection_info(stream::iter(vec![list_req]))
            .await
        {
            Err(_) => return Ok(GrpcInspectResult::Unreachable),
            Ok(response) => {
                let mut stream = response.into_inner();
                let mut names = vec![];
                while let Ok(Some(resp)) = stream.message().await {
                    if let Some(MessageResponse::ListServicesResponse(list)) = resp.message_response {
                        names = list.service.into_iter().map(|s| s.name).collect();
                        break;
                    }
                }
                names
            }
        };

        // Step 2: get file descriptors and count methods per service
        let mut method_counts: HashMap<String, usize> = HashMap::new();
        if !service_names.is_empty() {
            let fd_reqs: Vec<ServerReflectionRequest> = service_names
                .iter()
                .map(|name| ServerReflectionRequest {
                    host: String::new(),
                    message_request: Some(MessageRequest::FileContainingSymbol(name.clone())),
                })
                .collect();

            if let Ok(response) = client
                .server_reflection_info(stream::iter(fd_reqs))
                .await
            {
                let mut fd_stream = response.into_inner();
                while let Ok(Some(resp)) = fd_stream.message().await {
                    if let Some(MessageResponse::FileDescriptorResponse(fdr)) = resp.message_response {
                        for fd_bytes in fdr.file_descriptor_proto {
                            if let Ok(fd) = prost_types::FileDescriptorProto::decode(fd_bytes.as_slice()) {
                                let pkg = fd.package.as_deref().unwrap_or("");
                                for svc in &fd.service {
                                    let svc_name = svc.name.as_deref().unwrap_or("");
                                    let full_name = if pkg.is_empty() {
                                        svc_name.to_string()
                                    } else {
                                        format!("{pkg}.{svc_name}")
                                    };
                                    method_counts.entry(full_name).or_insert(svc.method.len());
                                }
                            }
                        }
                    }
                }
            }
        }

        let services = service_names
            .into_iter()
            .map(|name| {
                let method_count = *method_counts.get(&name).unwrap_or(&0);
                GrpcServiceInfo { name, method_count }
            })
            .collect();

        Ok(GrpcInspectResult::Reachable { services })
    }
}
