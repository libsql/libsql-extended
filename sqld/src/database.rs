use std::sync::Arc;

use crate::connection::libsql::LibSqlConnection;
use crate::connection::write_proxy::WriteProxyConnection;
use crate::connection::{Connection, MakeConnection, TrackedConnection};
use crate::replication::ReplicationLogger;

pub struct DatabaseInfo {
    current_frame_no: FrameNo,
}

pub trait Database: Sync + Send + 'static {
    /// The connection type of the database
    type Connection: Connection;

    fn connection_maker(&self) -> Arc<dyn MakeConnection<Connection = Self::Connection>>;
    fn shutdown(&self);
    fn info(&self) -> DatabaseInfo;
}

pub struct ReplicaDatabase {
    pub connection_maker:
        Arc<dyn MakeConnection<Connection = TrackedConnection<WriteProxyConnection>>>,
}

impl Database for ReplicaDatabase {
    type Connection = TrackedConnection<WriteProxyConnection>;

    fn connection_maker(&self) -> Arc<dyn MakeConnection<Connection = Self::Connection>> {
        self.connection_maker.clone()
    }

    fn shutdown(&self) {}

    fn info(&self) -> DatabaseInfo {
        DatabaseInfo { 
            current_frame_no: todo!()
        }
    }
}

pub struct PrimaryDatabase {
    pub logger: Arc<ReplicationLogger>,
    pub connection_maker: Arc<dyn MakeConnection<Connection = TrackedConnection<LibSqlConnection>>>,
}

impl Database for PrimaryDatabase {
    type Connection = TrackedConnection<LibSqlConnection>;

    fn connection_maker(&self) -> Arc<dyn MakeConnection<Connection = Self::Connection>> {
        self.connection_maker.clone()
    }

    fn shutdown(&self) {
        self.logger.closed_signal.send_replace(true);
    }

    fn info(&self) -> DatabaseInfo {
        DatabaseInfo { current_frame_no: todo!() }
    }
}
