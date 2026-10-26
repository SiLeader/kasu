use bytes::Bytes;
use http::{Request, Response};
use http_body::{Body, Frame, SizeHint};
use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{Layer, Service};
use tracing::{Instrument, Span, instrument::WithSubscriber};

#[derive(Clone, Default)]
pub struct GrpcLayer;
#[derive(Clone)]
pub struct GrpcService<S>(S);
impl<S> Layer<S> for GrpcLayer {
    type Service = GrpcService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        GrpcService(inner)
    }
}
impl<S: tonic::server::NamedService> tonic::server::NamedService for GrpcService<S> {
    const NAME: &'static str = S::NAME;
}
impl<S, B, R> Service<Request<B>> for GrpcService<S>
where
    S: Service<Request<B>, Response = Response<R>, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
    R: Body<Data = Bytes> + Send + 'static,
    R::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }
    fn call(&mut self, request: Request<B>) -> Self::Future {
        let clone = self.0.clone();
        let mut inner = std::mem::replace(&mut self.0, clone);
        let method = match request.uri().path().rsplit('/').next() {
            Some("GetTrees") => "GetTrees",
            Some("GetBlobs") => "GetBlobs",
            _ => "Unknown",
        };
        let span = crate::server_span(method, "grpc", |key| {
            let mut values = request.headers().get_all(key).iter();
            let first = values.next()?;
            if values.next().is_some() {
                return None;
            }
            first.to_str().ok()
        });
        let entered = span.clone();
        Box::pin(
            async move {
                let response = inner.call(request).await?;
                let (parts, body) = response.into_parts();
                let code = code(&parts.headers).unwrap_or(tonic::Code::Ok);
                Ok(Response::from_parts(
                    parts,
                    tonic::body::Body::new(TracedBody {
                        body: Box::pin(body),
                        span,
                        code,
                        done: false,
                    }),
                ))
            }
            .instrument(entered)
            .with_current_subscriber(),
        )
    }
}
fn code(headers: &http::HeaderMap) -> Option<tonic::Code> {
    headers
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i32>().ok())
        .filter(|v| (0..=16).contains(v))
        .map(tonic::Code::from_i32)
}
struct TracedBody<B: Body> {
    body: Pin<Box<B>>,
    span: Span,
    code: tonic::Code,
    done: bool,
}
impl<B: Body> Body for TracedBody<B> {
    type Data = B::Data;
    type Error = B::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let _entered = this.span.enter();
        match this.body.as_mut().poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(trailers) = frame.trailers_ref() {
                    this.code = code(trailers).unwrap_or(this.code);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(None) => {
                this.done = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                this.done = true;
                this.code = tonic::Code::Internal;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
impl<B: Body> Drop for TracedBody<B> {
    fn drop(&mut self) {
        crate::finish(
            &self.span,
            if self.done || self.body.is_end_stream() {
                self.code
            } else {
                tonic::Code::Cancelled
            },
        );
    }
}
