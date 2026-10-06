mod common;

use bytes::Bytes;
use common::e2e::{auth_disabled, LiveServer, AZURE_VERSION};
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use std::sync::Arc;

const PAYLOADS: [&str; 3] = ["{\"v\":1}\n", "{\"v\":100}\n", "{}\n"];

async fn put(server: &LiveServer, body: &'static str) {
    let response = server
        .request(
            Request::builder()
                .method("PUT")
                .uri(format!("{}/coherent-http/lease", server.base_url))
                .header("content-length", body.len())
                .body(Full::from(body))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body().collect().await.unwrap();
}

async fn verify_reads(server: &LiveServer, path: &str, headers: &[(&str, &str)]) {
    for iteration in 0..120 {
        let ranged = iteration % 2 == 1;
        let mut builder = Request::builder()
            .method("GET")
            .uri(format!("{}{path}", server.base_url))
            .header("content-length", "0");
        for (key, value) in headers {
            builder = builder.header(*key, *value);
        }
        if ranged {
            builder = builder.header("range", "bytes=0-");
        }
        let response = server
            .request(builder.body(Full::<Bytes>::default()).unwrap())
            .await;
        assert_eq!(
            response.status(),
            if ranged {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            }
        );
        let length: usize = response.headers()["content-length"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let etag = response.headers()["etag"]
            .to_str()
            .unwrap()
            .trim_matches('"')
            .to_string();
        let content_range = response
            .headers()
            .get("content-range")
            .map(|value| value.to_str().unwrap().to_string());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(length, body.len());
        assert!(
            PAYLOADS
                .iter()
                .any(|expected| expected.as_bytes() == body.as_ref()),
            "incomplete or mixed body: {body:?}"
        );
        assert_eq!(etag, format!("{:x}", md5::compute(&body)));
        if ranged {
            assert_eq!(
                content_range,
                Some(format!("bytes 0-{}/{}", body.len() - 1, body.len()))
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_return_coherent_object_generations_during_native_http_overwrites() {
    // Arrange
    let server = Arc::new(LiveServer::start_api(auth_disabled()).await);
    let response = server
        .request(
            Request::builder()
                .method("PUT")
                .uri(format!("{}/coherent-http", server.base_url))
                .body(Full::<Bytes>::default())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.into_body().collect().await.unwrap();
    put(&server, PAYLOADS[0]).await;

    // Act
    let barrier = Arc::new(tokio::sync::Barrier::new(6));
    let writer_server = server.clone();
    let writer_barrier = barrier.clone();
    let writer = tokio::spawn(async move {
        writer_barrier.wait().await;
        for iteration in 0..240 {
            put(&writer_server, PAYLOADS[iteration % 3]).await;
        }
    });
    let surfaces: [(&str, &[(&str, &str)]); 5] = [
        ("/coherent-http/lease", &[]),
        (
            "/devstoreaccount1/coherent-http/lease",
            &[("x-ms-version", AZURE_VERSION)],
        ),
        ("/storage/v1/b/coherent-http/o/lease?alt=media", &[]),
        (
            "/coherent-http/lease",
            &[("host", "storage.googleapis.com")],
        ),
        ("/n/sqrzl-emulator/b/coherent-http/o/lease", &[]),
    ];
    let readers: Vec<_> = surfaces
        .into_iter()
        .map(|(path, headers)| {
            let server = server.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                verify_reads(&server, path, headers).await;
            })
        })
        .collect();

    // Assert
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        writer.await.unwrap();
        for reader in readers {
            reader.await.unwrap();
        }
    })
    .await
    .expect("native HTTP control must make bounded progress");
}
