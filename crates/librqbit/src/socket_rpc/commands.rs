use librqbit_core::hash_id::Id20;

#[derive(Debug, Clone)]
pub enum WorkerCommand<'a> {
    AssignTorrent(&'a [u8]), // Infohash or magnet link bytes
    Pause(Id20),
    RequestTelemetry,
}

impl<'a> WorkerCommand<'a> {
    pub fn parse(buf: &'a [u8]) -> anyhow::Result<Self> {
        if buf.is_empty() {
            anyhow::bail!("empty command");
        }
        match buf[0] {
            0x01 => {
                Ok(WorkerCommand::AssignTorrent(&buf[1..]))
            }
            0x02 => {
                if buf.len() != 21 {
                    anyhow::bail!("invalid pause command length");
                }
                let mut info_hash = [0u8; 20];
                info_hash.copy_from_slice(&buf[1..21]);
                Ok(WorkerCommand::Pause(Id20::new(info_hash)))
            }
            0x03 => {
                Ok(WorkerCommand::RequestTelemetry)
            }
            _ => anyhow::bail!("unknown command"),
        }
    }
}
