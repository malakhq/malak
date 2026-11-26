//! A lead and two helpers in one process: no socket, no command line.
//!
//! Run it with `cargo run -p malak --example team --no-default-features`.
//!
//! The lead sends each helper a task. One helper reports back; the other asks a
//! question, which the lead answers. The program checks every step and stops the
//! board at the end.

use malak::{
    Action, Error, Grant, Handle, Header, HeaderValue, Kind, Message, Owner, ReadReply, Receipt,
    SendRequest,
};
use std::thread;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

/// Enrol a helper. It may report to the lead, ask and read answers on `questions`,
/// and read its own inbox, which exists once it is enrolled.
fn enrol_helper(owner: &Owner, name: &str) -> Result<Handle, Error> {
    let mut grant = Grant::default();
    grant.allow("lead".parse()?, [Action::Send]);
    grant.allow(
        "questions".parse()?,
        [Action::Send, Action::Read, Action::Ack],
    );
    let helper = owner.enrol(name, grant.clone())?;
    grant.allow(
        helper.id().inbox().as_str().parse()?,
        [Action::Read, Action::Ack],
    );
    owner.replace_grant(helper.id(), grant)?;
    Ok(helper)
}

/// Wait for the next message in `channel`, and acknowledge it.
fn next_message(handle: &Handle, channel: &str) -> Result<Message, Error> {
    let reply: ReadReply = handle.read(&[channel], 1, WAIT)?;
    let message = reply
        .messages
        .into_iter()
        .next()
        .expect("a message within the wait");
    handle.ack(
        channel,
        message.receipt.creation,
        message.receipt.position.get(),
    )?;
    Ok(message)
}

/// Whether `message` carries a reply-to header that names `question`.
fn replies_to(message: &Message, question: &Receipt) -> bool {
    message.headers.iter().any(|header| {
        matches!(header, Header::ReplyTo { creation, position, .. }
            if *creation == question.creation && *position == question.position)
    })
}

fn main() -> Result<(), Error> {
    let owner: Owner = malak::in_memory()?;
    let lead = owner.lead();
    // The board holds only the lead's inbox at first. Make the channel for questions.
    owner.create_channel("questions")?;
    let ada = enrol_helper(&owner, "ada")?;
    let ben = enrol_helper(&owner, "ben")?;

    // Send: the lead gives each helper a task in its own inbox.
    for helper in [&ada, &ben] {
        let inbox = helper.id().inbox();
        let task = SendRequest {
            kind: Kind::Task,
            ..SendRequest::note(inbox.as_str(), "task-1", b"run the checks")
        };
        lead.send(task)?;
    }

    thread::scope(|scope| -> Result<(), Error> {
        // Read and ack: ada takes her task and reports to the lead.
        let ada_thread = scope.spawn(|| -> Result<(), Error> {
            let task = next_message(&ada, ada.id().inbox().as_str())?;
            assert_eq!(&task.body[..], b"run the checks");
            ada.send(SendRequest {
                kind: Kind::Report,
                ..SendRequest::note("lead", "report-1", b"checks passed")
            })?;
            Ok(())
        });

        // A question: ben takes his task, then asks the owner what to do.
        let ben_thread = scope.spawn(|| -> Result<Message, Error> {
            next_message(&ben, ben.id().inbox().as_str())?;
            let question = ben.send(SendRequest::note(
                "questions",
                "question-1",
                b"Which build?",
            ))?;
            // Wait for the answer: the message on `questions` that replies to his question.
            let answer = loop {
                let message = next_message(&ben, "questions")?;
                if replies_to(&message, &question) {
                    break message;
                }
            };
            Ok(answer)
        });

        // The lead reads the report and the question, and acknowledges both.
        let report = next_message(&lead, "lead")?;
        assert_eq!(&report.body[..], b"checks passed");
        assert_eq!(report.sender, *ada.id());

        // An answer: the lead replies on `questions`, naming the question it answers.
        let question = next_message(&lead, "questions")?;
        assert_eq!(&question.body[..], b"Which build?");
        let reply_to = [(
            "reply_to",
            HeaderValue::ReplyTo {
                channel: "questions",
                creation: question.receipt.creation,
                position: question.receipt.position.get(),
            },
        )];
        lead.send(SendRequest {
            headers: &reply_to,
            ..SendRequest::note("questions", "answer-1", b"The stable build.")
        })?;

        ada_thread.join().expect("ada finished")?;
        let answer = ben_thread.join().expect("ben finished")?;
        assert_eq!(&answer.body[..], b"The stable build.");
        Ok(())
    })?;

    // Stop: read the run, then stop it. Dropping the handles ends the board.
    let status = owner.status(None, None);
    owner.stop(status.run)?;
    println!("lead, ada and ben finished; the board has stopped");
    Ok(())
}
