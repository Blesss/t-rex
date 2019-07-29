//
// Copyright (c) Pirmin Kalberer. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root for full license information.
//

use crate::core::config::ApplicationCfg;
use crate::mvt_service::MvtService;
use crate::runtime_config::{config_from_args, service_from_args};
use crate::static_files::StaticFiles;
use actix_cors::Cors;
use actix_files as fs;
use actix_rt;
use actix_web::http::{header, ContentEncoding};
use actix_web::middleware::{BodyEncoding, Compress};
use actix_web::{middleware, web, App, Error, HttpRequest, HttpResponse, HttpServer, Result};
use clap::ArgMatches;
use futures::{future::ok, Future};
use log::Level;
use open;
use std::collections::HashMap;
use std::str;
use std::str::FromStr;

static DINO: &'static str = "             xxxxxxxxx
        xxxxxxxxxxxxxxxxxxxxxxxx
      xxxxxxxxxxxxxxxxxxxxxxxxxxxx
     xxxxxxxxxxxxxxxxxxxxxxxxx xxxx
     xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
   xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
 xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx xxxxxxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx  xxxxxxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx   xxxxxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx   xxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx     xxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx      x
xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
xxxxxxxxxxxxxxxxxxxxxxxxxx    xxxxxxxxxxx
xxxxxxxxxxxxxx                   xxxxxx
xxxxxxxxxxxx
xxxxxxxxxxx
xxxxxxxxxx
xxxxxxxxx
xxxxxxx
xxxxxx
xxxxxxx";

fn mvt_metadata(service: web::Data<MvtService>) -> impl Future<Item = HttpResponse, Error = Error> {
    let json = service.get_mvt_metadata().unwrap();
    ok(HttpResponse::Ok().json(json))
}

/// Font list for Maputnik
fn fontstacks() -> Result<HttpResponse> {
    Ok(HttpResponse::Ok().json(["Roboto Medium", "Roboto Regular"]))
}

// Include method fonts() which returns HashMap with embedded font files
include!(concat!(env!("OUT_DIR"), "/fonts.rs"));

/// Fonts for Maputnik
/// Example: /fonts/Open%20Sans%20Regular,Arial%20Unicode%20MS%20Regular/0-255.pbf
fn fonts_pbf(params: web::Path<(String, String)>) -> Result<HttpResponse, Error> {
    let fontpbfs = fonts();
    let fontlist = &params.0;
    let range = &params.1;
    let mut fonts = fontlist.split(",").collect::<Vec<_>>();
    fonts.push("Roboto Regular"); // Fallback
    let mut resp = HttpResponse::NotFound().finish();
    for font in fonts {
        let key = format!("fonts/{}/{}.pbf", font.replace("%20", " "), range);
        debug!("Font lookup: {}", key);
        if let Some(pbf) = fontpbfs.get(&key as &str) {
            resp = HttpResponse::Ok()
                .content_type("application/x-protobuf")
                // data is already gzip compressed
                .encoding(ContentEncoding::Identity)
                .header(header::CONTENT_ENCODING, "gzip")
                .body(*pbf); // TODO: chunked response
            break;
        }
    }
    Ok(resp)
}

fn req_baseurl(req: &HttpRequest) -> String {
    let conninfo = req.connection_info();
    format!("{}://{}", conninfo.scheme(), conninfo.host())
}

fn tileset_tilejson(
    service: web::Data<MvtService>,
    tileset: web::Path<String>,
    req: HttpRequest,
) -> impl Future<Item = HttpResponse, Error = Error> {
    let json = service.get_tilejson(&req_baseurl(&req), &tileset).unwrap();
    ok(HttpResponse::Ok().json(json))
}

fn tileset_style_json(
    service: web::Data<MvtService>,
    tileset: web::Path<String>,
    req: HttpRequest,
) -> impl Future<Item = HttpResponse, Error = Error> {
    let json = service.get_stylejson(&req_baseurl(&req), &tileset).unwrap();
    ok(HttpResponse::Ok().json(json))
}

fn tileset_metadata_json(
    service: web::Data<MvtService>,
    tileset: web::Path<String>,
) -> impl Future<Item = HttpResponse, Error = Error> {
    let json = service.get_mbtiles_metadata(&tileset).unwrap();
    ok(HttpResponse::Ok().json(json))
}

fn tile_pbf(
    config: web::Data<ApplicationCfg>,
    service: web::Data<MvtService>,
    params: web::Path<(String, u8, u32, u32)>,
    req: HttpRequest,
) -> impl Future<Item = HttpResponse, Error = Error> {
    let tileset = &params.0;
    let z = params.1;
    let x = params.2;
    let y = params.3;
    let gzip = req
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|headerval| {
            headerval
                .to_str()
                .ok()
                .and_then(|headerstr| Some(headerstr.contains("gzip")))
        })
        .unwrap_or(false);
    let tile = service.tile_cached(tileset, x, y, z, gzip, None);
    let cache_max_age = config.webserver.cache_control_max_age.unwrap_or(300);

    let resp = if let Some(tile) = tile {
        HttpResponse::Ok()
            .content_type("application/x-protobuf")
            .if_true(gzip, |r| {
                // data is already gzip compressed
                r.encoding(ContentEncoding::Identity)
                    .header(header::CONTENT_ENCODING, "gzip");
            })
            .header(header::CACHE_CONTROL, format!("max-age={}", cache_max_age))
            .body(tile) // TODO: chunked response
    } else {
        HttpResponse::NoContent().finish()
    };
    ok(resp)
}

lazy_static! {
    static ref STATIC_FILES: StaticFiles = StaticFiles::init();
}

fn static_file_handler(req: HttpRequest) -> Result<HttpResponse, Error> {
    let key = req.path()[1..].to_string();
    let resp = if let Some(ref content) = STATIC_FILES.content(None, key) {
        HttpResponse::Ok()
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*") // TOOD: use Actix middleware
            .content_type(content.1)
            .body(content.0) // TODO: chunked response
    } else {
        HttpResponse::NotFound().finish()
    };
    Ok(resp)
}

#[derive(Deserialize)]
struct DrilldownParams {
    minzoom: Option<u8>,
    maxzoom: Option<u8>,
    points: String, //x1,y1,x2,y2,..
}

fn drilldown_handler(
    service: web::Data<MvtService>,
    params: web::Query<DrilldownParams>,
) -> impl Future<Item = HttpResponse, Error = Error> {
    let tileset = None; // all tilesets
    let progress = false;
    let points: Vec<f64> = params
        .points
        .split(",")
        .map(|v| {
            v.parse()
                .expect("Error parsing 'point' as pair of float values")
            //FIXME: map_err(|_| error::ErrorInternalServerError("...")
        })
        .collect();
    let stats = service.drilldown(tileset, params.minzoom, params.maxzoom, points, progress);
    let json = stats.as_json().unwrap();
    ok(HttpResponse::Ok().json(json))
}

pub fn webserver(args: ArgMatches<'static>) {
    let config = config_from_args(&args);
    let host = config
        .webserver
        .bind
        .clone()
        .unwrap_or("127.0.0.1".to_string());
    let port = config.webserver.port.unwrap_or(6767);
    let bind_addr = format!("{}:{}", host, port);
    let mvt_viewer = config.service.mvt.viewer;
    let openbrowser =
        bool::from_str(args.value_of("openbrowser").unwrap_or("true")).unwrap_or(false);
    let static_dirs = config.webserver.static_.clone();

    let mut service = service_from_args(&config, &args);
    service.prepare_feature_queries();
    service.init_cache();

    let sys = actix_rt::System::new("t-rex");

    HttpServer::new(move || {
        let mut app = App::new()
            .data(config.clone())
            .data(service.clone())
            .wrap(middleware::Logger::new("%r %s %b %Dms %a"))
            .wrap(Compress::default())
            .wrap(Cors::new().send_wildcard().allowed_methods(vec!["GET"]))
            .service(web::resource("/index.json").route(web::get().to_async(mvt_metadata)))
            .service(web::resource("/fontstacks.json").route(web::get().to(fontstacks)))
            .service(web::resource("/fonts/{fonts}/{range}.pbf").route(web::get().to(fonts_pbf)))
            .service(
                web::resource("/{tileset}.style.json")
                    .route(web::get().to_async(tileset_style_json)),
            )
            .service(
                web::resource("/{tileset}/metadata.json")
                    .route(web::get().to_async(tileset_metadata_json)),
            )
            .service(web::resource("/{tileset}.json").route(web::get().to_async(tileset_tilejson)))
            .service(
                web::resource("/{tileset}/{z}/{x}/{y}.pbf").route(web::get().to_async(tile_pbf)),
            );
        for static_dir in &static_dirs {
            let dir = &static_dir.dir;
            if std::path::Path::new(dir).is_dir() {
                info!("Serving static files from directory '{}'", dir);
                app = app.service(fs::Files::new(&static_dir.path, dir));
            } else {
                warn!("Static file directory '{}' not found", dir);
            }
        }
        if mvt_viewer {
            app = app
                .service(web::resource("/drilldown").route(web::get().to_async(drilldown_handler)));
            app = app.default_service(web::to(static_file_handler));
        }
        app
    })
    .bind(&bind_addr)
    .expect("Can not start server on given IP/Port")
    .shutdown_timeout(3) // default: 30s
    .start();

    if log_enabled!(Level::Info) {
        println!("{}", DINO);
    }

    if openbrowser && mvt_viewer {
        let _res = open::that(format!("http://{}:{}", &host, port));
    }

    sys.run().expect("Couldn't run HttpServer");
}
