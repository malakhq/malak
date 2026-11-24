use malak_layers::{ProcessIdentity, SenderTrace, SessionProcess};
use minicbor::{Decoder, Encoder, decode, encode::Write};

/// The local process and optional agent identifier recorded for the lead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeadSession {
    /// The OS-observed session that sent the message.
    pub process: SessionProcess,
    /// The agent tool's reported session identifier, when available.
    pub agent_session_id: Option<Box<str>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl LeadSession {
    pub(crate) fn write<W: Write>(&self, out: &mut Encoder<W>)
    where
        W::Error: std::fmt::Debug,
    {
        write_session(out, &self.process, self.agent_session_id.as_deref());
    }

    pub(crate) fn read(input: &mut Decoder<'_>) -> Result<Self, decode::Error> {
        read_session(input)
    }
}

pub(crate) fn write<W: Write>(
    out: &mut Encoder<W>,
    sender: &SenderTrace,
    earlier: Option<&SessionProcess>,
    agent_session_id: Option<&str>,
) where
    W::Error: std::fmt::Debug,
{
    let fields = 1
        + u64::from(sender.pid.is_some())
        + u64::from(sender.program.is_some())
        + u64::from(sender.cwd.is_some())
        + u64::from(sender.session.is_some())
        + u64::from(earlier.is_some());
    out.map(fields).unwrap();
    out.str("transport")
        .unwrap()
        .str(&sender.transport)
        .unwrap();
    if let Some(pid) = sender.pid {
        out.str("pid").unwrap().u32(pid).unwrap();
    }
    if let Some(program) = &sender.program {
        out.str("program").unwrap().str(program).unwrap();
    }
    if let Some(cwd) = &sender.cwd {
        out.str("cwd").unwrap().str(cwd).unwrap();
    }
    for (label, session) in [
        ("session", sender.session.as_ref()),
        ("earlier_session", earlier),
    ] {
        if let Some(session) = session {
            out.str(label).unwrap();
            write_session(
                out,
                session,
                if label == "session" {
                    agent_session_id
                } else {
                    None
                },
            );
        }
    }
}

pub(crate) fn read(
    input: &mut Decoder<'_>,
) -> Result<(SenderTrace, Option<Box<str>>), decode::Error> {
    let mut trace = SenderTrace::transport("");
    let mut agent_session_id = None;
    for _ in 0..fields(input)? {
        match input.str()? {
            "transport" => trace.transport = input.str()?.into(),
            "pid" => trace.pid = Some(input.u32()?),
            "program" => trace.program = Some(input.str()?.into()),
            "cwd" => trace.cwd = Some(input.str()?.into()),
            "session" => {
                let session = read_session(input)?;
                trace.session = Some(session.process);
                agent_session_id = session.agent_session_id;
            }
            _ => input.skip()?,
        }
    }
    if trace.transport.is_empty() {
        return Err(decode::Error::message("sender transport missing"));
    }
    Ok((trace, agent_session_id))
}

fn write_session<W: Write>(
    out: &mut Encoder<W>,
    process: &SessionProcess,
    agent_session_id: Option<&str>,
) where
    W::Error: std::fmt::Debug,
{
    out.map(3 + u64::from(agent_session_id.is_some())).unwrap();
    out.str("pid").unwrap().u32(process.identity.pid).unwrap();
    out.str("program").unwrap().str(&process.program).unwrap();
    out.str("start_time")
        .unwrap()
        .u64(process.identity.start_time)
        .unwrap();
    if let Some(id) = agent_session_id {
        out.str("agent_session_id").unwrap().str(id).unwrap();
    }
}

fn read_session(input: &mut Decoder<'_>) -> Result<LeadSession, decode::Error> {
    let mut pid = None;
    let mut start_time = None;
    let mut program = None;
    let mut agent_session_id = None;
    for _ in 0..fields(input)? {
        match input.str()? {
            "pid" => pid = Some(input.u32()?),
            "start_time" => start_time = Some(input.u64()?),
            "program" => program = Some(input.str()?.into()),
            "agent_session_id" => agent_session_id = Some(input.str()?.into()),
            _ => input.skip()?,
        }
    }
    match (pid, start_time, program) {
        (Some(pid), Some(start_time), Some(program)) => Ok(LeadSession {
            process: SessionProcess {
                identity: ProcessIdentity { pid, start_time },
                program,
            },
            agent_session_id,
        }),
        _ => Err(decode::Error::message("incomplete session process")),
    }
}

fn fields(input: &mut Decoder<'_>) -> Result<u64, decode::Error> {
    input
        .map()?
        .ok_or_else(|| decode::Error::message("expected definite sender map"))
}
