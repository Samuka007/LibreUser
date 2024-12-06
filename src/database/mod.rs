use crate::error::ServiceError;
use actix_web::web;
use diesel::r2d2::PooledConnection;
use diesel::PgConnection;

pub type DbPool = diesel::r2d2::Pool<diesel::r2d2::ConnectionManager<PgConnection>>;

pub mod error;
pub mod schema;

pub async fn get_conn(
    pool: web::Data<DbPool>,
) -> Result<PooledConnection<diesel::r2d2::ConnectionManager<PgConnection>>, ServiceError> {
    Ok(web::block(move || pool.get()).await??)
}
