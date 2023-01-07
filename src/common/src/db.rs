use futures_util::{TryFuture, TryStreamExt};
use prost::Message;
use proto::common::mysql_desc;
use sqlx::{Arguments, ConnectOptions};

use crate::types::TypedValue;

/// Connection of MySQL
/// Examples of usage: 
/// ```
/// use common::db::MysqlConn;
/// use proto::common::mysql_desc;
///
/// async fn main() {
///     let opts = mysql_desc::ConnectionOpts {
///         host: "localhost".to_string(),
///         port: 3306,
///         username: "root".to_string(),
///         password: "pwd".to_string()
///     };
///
///     let conn = MysqlConn::from(opts);
///}
/// ```
#[derive(Clone)]
pub struct MysqlConn {
    conn_opts: mysql_desc::ConnectionOpts,
}

impl MysqlConn {
    pub async fn execute(
        &self,
        statement: &str,
        arguments: Vec<TypedValue>,
        conn: &mut sqlx::mysql::MySqlConnection,
    ) -> Result<sqlx::mysql::MySqlQueryResult, sqlx::Error> {
        let mut mysql_arg = sqlx::mysql::MySqlArguments::default();
        arguments.iter().for_each(|val| match val {
            TypedValue::String(v) => mysql_arg.add(v),
            TypedValue::BigInt(v) => mysql_arg.add(v),
            TypedValue::Boolean(v) => mysql_arg.add(v),
            TypedValue::Number(v) => mysql_arg.add(v),
            _ => {}
        });

        sqlx::query_with(statement, mysql_arg).execute(conn).await
    }

    pub async fn try_for_each<
        Fut: TryFuture<Ok = (), Error = sqlx::Error>,
        F: FnMut(sqlx::mysql::MySqlRow) -> Fut,
    >(
        &self,
        statement: &str,
        arguments: Vec<TypedValue>,
        conn: &mut sqlx::mysql::MySqlConnection,
        mut f: F,
    ) -> Result<(), sqlx::Error> {
        let mut mysql_arg = sqlx::mysql::MySqlArguments::default();
        arguments.iter().for_each(|val| match val {
            TypedValue::String(v) => mysql_arg.add(v),
            TypedValue::BigInt(v) => mysql_arg.add(v),
            TypedValue::Boolean(v) => mysql_arg.add(v),
            TypedValue::Number(v) => mysql_arg.add(v),
            _ => {}
        });

        sqlx::query_with(statement, mysql_arg)
            .fetch(conn)
            .try_for_each(|row| f(row))
            .await
    }

    pub async fn connect(&self) -> Result<sqlx::mysql::MySqlConnection, sqlx::Error> {
        let opts = sqlx::mysql::MySqlConnectOptions::new()
            .host(&self.conn_opts.host)
            .port(3306)
            .username(&self.conn_opts.username)
            .password(&self.conn_opts.password)
            .database(&self.conn_opts.database);

        opts.connect().await
    }

    pub fn close(&mut self) {
        self.conn_opts.clear()
    }
}

impl From<mysql_desc::ConnectionOpts> for MysqlConn {
    fn from(conn_opts: mysql_desc::ConnectionOpts) -> Self {
        Self { conn_opts }
    }
}
