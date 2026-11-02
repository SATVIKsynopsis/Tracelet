use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{
    LogsService, LogsServiceServer,
};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use tonic::{Request, Response, Status};

use crate::store::{Payload, Protocol, Signal, Store};
use crate::{EXPORTER_HEADER, MAX_REQUEST_BYTES};

#[derive(Clone)]
pub(crate) struct GrpcCapture(pub(crate) Store);

impl GrpcCapture {
    fn record<T>(
        &self,
        signal: Signal,
        request: Request<T>,
        wrap: impl FnOnce(T) -> Payload,
    ) -> Result<(), Status> {
        let slug = request
            .metadata()
            .get(EXPORTER_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let Some(slug) = slug else {
            let reason = format!("gRPC {signal} export without a {EXPORTER_HEADER} header");
            self.0.reject(reason.clone());
            return Err(Status::invalid_argument(reason));
        };
        let Some(exporter) = self.0.exporter_for(&slug) else {
            let reason = format!("no exporter registered for {EXPORTER_HEADER}: {slug}");
            self.0.reject(reason.clone());
            return Err(Status::not_found(reason));
        };
        self.0
            .record(exporter, Protocol::Grpc, wrap(request.into_inner()));
        Ok(())
    }
}

#[tonic::async_trait]
impl TraceService for GrpcCapture {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        self.record(Signal::Traces, request, Payload::Traces)?;
        Ok(Response::new(ExportTraceServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl MetricsService for GrpcCapture {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        self.record(Signal::Metrics, request, Payload::Metrics)?;
        Ok(Response::new(ExportMetricsServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl LogsService for GrpcCapture {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        self.record(Signal::Logs, request, Payload::Logs)?;
        Ok(Response::new(ExportLogsServiceResponse::default()))
    }
}

pub(crate) fn services(
    store: Store,
) -> (
    TraceServiceServer<GrpcCapture>,
    MetricsServiceServer<GrpcCapture>,
    LogsServiceServer<GrpcCapture>,
) {
    let svc = GrpcCapture(store);
    (
        TraceServiceServer::new(svc.clone()).max_decoding_message_size(MAX_REQUEST_BYTES),
        MetricsServiceServer::new(svc.clone()).max_decoding_message_size(MAX_REQUEST_BYTES),
        LogsServiceServer::new(svc).max_decoding_message_size(MAX_REQUEST_BYTES),
    )
}
