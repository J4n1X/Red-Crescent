use actix_web::{App, HttpServer, web as actix_web_web};
use crate::lua_runtime::ManagedLuaInstance;
use crate::web::handlers::{lua_handler, lua_post_handler};

extern crate pretty_env_logger;

mod config;
mod lua_runtime;
mod web;

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    pretty_env_logger::init();
    log::info!("Starting up the lua html renderer server...");

    HttpServer::new(|| {
        App::new()
            // Attach a lua instance to each app instance
            .app_data(actix_web_web::Data::new(ManagedLuaInstance::new().unwrap()))
            // Handle all requests with lua_handler
            .service(
                actix_web_web::resource("/{tail:.*}")
                    .route(actix_web_web::get().to(lua_handler))
                    .route(actix_web_web::post().to(lua_post_handler))
            )
    })
    .workers(config::NUM_WORKERS)
    .bind((config::BIND_ADDRESS, config::BIND_PORT))?
    .run()
    .await
}
