//! A library's hero image: chosen as a site look's header image is, kept in the library's
//! record in Git, answered with the library, served at the brand image route while the
//! library pins it, taken away with `null`, refused without `manage_site`, put back from Git
//! at start, and released when the library is purged.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::Cursor;

use common::{Site, StoppableServer, TestUser};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use records::test_support::TestDb;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

async fn call(
    site: &Site,
    user: &TestUser,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = site.http(Some(user)).request(method, site.url(path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    let body = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    };
    (status, body)
}

async fn upload(site: &Site, user: &TestUser, parent: &str, name: &str, content: &[u8]) -> String {
    let response = site
        .http(Some(user))
        .post(site.url("/api/docs/upload"))
        .query(&[("parent", parent), ("name", name)])
        .body(content.to_vec())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, StatusCode::CREATED, "{name}: {body}");
    body["id"].as_str().unwrap().to_owned()
}

fn png(width: u32, height: u32, shade: u8) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, _| Rgb([shade, (x % 256) as u8, 90]));
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(image)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    out
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn new_library(site: &Site, admin: &TestUser, site_id: &str, name: &str) -> String {
    let (status, body) = call(
        site,
        admin,
        Method::POST,
        &format!("/api/sites/{site_id}/libraries"),
        Some(json!({ "name": name, "kind": "image" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_owned()
}

async fn hero(site: &Site, user: &TestUser, library: &str, image: Value) -> (StatusCode, Value) {
    call(
        site,
        user,
        Method::PATCH,
        &format!("/api/libraries/{library}"),
        Some(json!({ "hero_image": image })),
    )
    .await
}

async fn image_status(site: &Site, user: &TestUser, url: &str) -> StatusCode {
    site.http(Some(user))
        .get(site.url(url))
        .send()
        .await
        .unwrap()
        .status()
}

/// The owners pinning `sha256` in `brand_refs`.
async fn pin_owners(db: &TestDb, sha256: &str) -> Vec<String> {
    sqlx::query_scalar("select owner from brand_refs where sha256 = $1 order by owner")
        .bind(sha256)
        .fetch_all(db.db.test_pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn library_hero_image_chosen_kept_and_released() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let mut first = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = first.admin("Root").await;
    let (status, body) = call(
        &first,
        &root,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": "Harbour" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let site = body["id"].as_str().unwrap().to_owned();
    let pictures = new_library(&first, &root, &site, "Pictures").await;
    let shown = new_library(&first, &root, &site, "Shown").await;
    let wide = png(1200, 300, 40);
    let doc = upload(&first, &root, &pictures, "Pier.png", &wide).await;

    // None at first.
    let (status, body) = call(
        &first,
        &root,
        Method::GET,
        &format!("/api/libraries/{shown}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hero_image"], Value::Null);

    // Chosen from a document of the site: answered with the library, kept in Git, pinned under
    // the library's id and served at the brand image route.
    let (status, body) = hero(&first, &root, &shown, json!({ "doc": doc })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let url = format!("/api/brand/images/{}", sha(&wide));
    assert_eq!(
        body["hero_image"],
        json!({ "url": url, "name": "Pier.png", "sha256": sha(&wide), "width": 1200, "height": 300 })
    );
    let store = revisions::RevisionStore::open(&server::library_path(data_dir.path())).unwrap();
    let record = store
        .read_library_record(Uuid::parse_str(&shown).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.hero_image.as_ref().unwrap().sha256, sha(&wide));
    assert_eq!(record.kind, "image");
    assert_eq!(pin_owners(&db, &sha(&wide)).await, vec![shown.clone()]);
    assert_eq!(image_status(&first, &root, &url).await, StatusCode::OK);
    // Its banner-sized picture: a JPEG, no larger than it needs to be (this one is 4:1 and
    // narrower than 2400, so as wide and high as it is); any other size is refused.
    let response = first
        .http(Some(&root))
        .get(first.url(&format!("{url}?size=banner")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/jpeg");
    let bytes = response.bytes().await.unwrap();
    let banner = image::load_from_memory(&bytes).unwrap();
    assert_eq!((banner.width(), banner.height()), (1200, 300));
    assert_eq!(
        image_status(&first, &root, &format!("{url}?size=huge")).await,
        StatusCode::BAD_REQUEST
    );
    // Kept as it is; the same answer.
    let (status, again) = hero(&first, &root, &shown, json!({ "keep": sha(&wide) })).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["hero_image"], body["hero_image"]);

    // Who may not change the library may not choose it; nor a document that is not an image,
    // nor a slot that names two ways.
    let member = first.user("Mia").await;
    let (status, body) = call(
        &first,
        &root,
        Method::PUT,
        &format!("/api/nodes/{site}/access"),
        Some(json!({ "email": member.email, "role": "editor" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = hero(&first, &member, &shown, Value::Null).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let words = upload(&first, &root, &pictures, "Notes.svg", b"not an svg").await;
    let (status, refused) = hero(&first, &root, &shown, json!({ "doc": words })).await;
    assert!(status.is_client_error(), "{status} {refused}");
    let (status, _) = hero(
        &first,
        &root,
        &shown,
        json!({ "doc": doc, "keep": sha(&wide) }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, body) = call(
        &first,
        &root,
        Method::GET,
        &format!("/api/libraries/{shown}"),
        None,
    )
    .await;
    assert_eq!(body["hero_image"]["sha256"], sha(&wide));

    // Replaced: the old content is no longer pinned (its document still holds it).
    let tall = png(800, 400, 200);
    let other = upload(&first, &root, &pictures, "Lighthouse.png", &tall).await;
    let (status, body) = hero(&first, &root, &shown, json!({ "doc": other })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hero_image"]["sha256"], sha(&tall));
    assert!(pin_owners(&db, &sha(&wide)).await.is_empty());
    assert_eq!(
        image_status(&first, &root, &url).await,
        StatusCode::NOT_FOUND
    );

    // Lost from the database while stopped: put back from Git at start.
    first.stop();
    sqlx::query("update libraries set hero_image = null where id = $1")
        .bind(Uuid::parse_str(&shown).unwrap())
        .execute(db.db.test_pool())
        .await
        .unwrap();
    sqlx::query("delete from brand_refs where owner = $1")
        .bind(&shown)
        .execute(db.db.test_pool())
        .await
        .unwrap();
    let second = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = second.admin("Rita").await;
    let (_, body) = call(
        &second,
        &root,
        Method::GET,
        &format!("/api/libraries/{shown}"),
        None,
    )
    .await;
    assert_eq!(body["hero_image"]["sha256"], sha(&tall), "{body}");
    assert_eq!(pin_owners(&db, &sha(&tall)).await, vec![shown.clone()]);

    // Taken away with null.
    let (status, body) = hero(&second, &root, &shown, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hero_image"], Value::Null);
    assert!(pin_owners(&db, &sha(&tall)).await.is_empty());

    // Purged with its library: the pin goes too.
    let (status, body) = hero(&second, &root, &shown, json!({ "doc": doc })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &second,
        &root,
        Method::DELETE,
        &format!("/api/nodes/{shown}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    // In the bin it still pins its image.
    assert_eq!(pin_owners(&db, &sha(&wide)).await, vec![shown.clone()]);
    for _ in 0..2 {
        let (status, body) = call(
            &second,
            &root,
            Method::DELETE,
            &format!("/api/bin/{shown}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
    assert!(pin_owners(&db, &sha(&wide)).await.is_empty());
    // The document's own version still holds the content.
    let blobs = revisions::BlobStore::open(&server::blobs_path(data_dir.path())).unwrap();
    assert!(blobs.contains(&Sha256::digest(&wide).into()));
}

/// With images from the site's own libraries turned off for site owners, a site owner may give
/// a library only a brand-centre picture as its banner (as on Change the look); an
/// administrator may still give it any.
#[tokio::test]
async fn library_banner_follows_the_brand_image_limit() {
    let data_dir = tempfile::tempdir().unwrap();
    let db = TestDb::create().await;
    let server = StoppableServer::start(data_dir.path(), &db, |_| {});
    let root = server.admin("Root").await;
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        "/api/sites",
        Some(json!({ "name": "Quay" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let site = body["id"].as_str().unwrap().to_owned();
    let pictures = new_library(&server, &root, &site, "Pictures").await;
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        &format!("/api/sites/{site}/libraries"),
        Some(json!({ "name": "Brand", "kind": "asset" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let assets = body["id"].as_str().unwrap().to_owned();
    let own = upload(&server, &root, &pictures, "Quay.png", &png(900, 300, 30)).await;
    let branded = png(1000, 400, 120);
    let brand_doc = upload(&server, &root, &assets, "Brand banner.png", &branded).await;
    let (status, body) = call(
        &server,
        &root,
        Method::POST,
        "/api/brand/images",
        Some(json!({ "doc": brand_doc, "kind": "background" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let brand_image = body["id"].as_str().unwrap().to_owned();
    let olive = server.user("Olive").await;
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        &format!("/api/nodes/{site}/access"),
        Some(json!({ "email": olive.email, "role": "owner" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // The limit off: a site owner may choose a picture from the site's libraries.
    let (status, body) = hero(&server, &olive, &pictures, json!({ "doc": own })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The limit on: refused, and the banner stays as it was.
    let (status, body) = call(
        &server,
        &root,
        Method::PUT,
        "/api/brand/policy",
        Some(json!({ "site_images": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let other = upload(&server, &root, &pictures, "Other.png", &png(800, 300, 60)).await;
    let (status, body) = hero(&server, &olive, &pictures, json!({ "doc": other })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "not_allowed", "{body}");
    let (_, body) = call(
        &server,
        &olive,
        Method::GET,
        &format!("/api/libraries/{pictures}"),
        None,
    )
    .await;
    assert_eq!(body["hero_image"]["name"], "Quay.png");
    // A brand-centre image is allowed.
    let (status, body) = hero(
        &server,
        &olive,
        &pictures,
        json!({ "brand_image": brand_image }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["hero_image"]["sha256"], sha(&branded));
    // Taking it away is never refused.
    let (status, _) = hero(&server, &olive, &pictures, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    // An administrator is not limited.
    let (status, body) = hero(&server, &root, &pictures, json!({ "doc": other })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
