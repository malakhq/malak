use super::*;
use malak_core::CreationId;
use malak_layers::{Error, Link, Received, Source, TransportInfo};
use malak_transport_unix::UnixLink;

const ADDRESS: &str = "vsock:2:4100";

#[test]
fn a_vm_bound_key_attaches_only_from_its_original_vm_after_restart() {
    let fixture = Fixture::new();
    let (board, entrances) = fixture.start_on(Entrance::General, ADDRESS).unwrap();
    let original = CreationId::from_bytes([1; 16]);
    let clone = CreationId::from_bytes([2; 16]);
    let path = fixture.root.path().join("worker.handover");
    let cloned_path = fixture.root.path().join("cloned.handover");
    entrances
        .enrol_for_vm("worker", Grant::default(), &path, original)
        .unwrap();
    fs::copy(&path, &cloned_path).unwrap();
    drop(entrances);
    drop(board);

    let (_board, entrances) = fixture.start_on(Entrance::General, ADDRESS).unwrap();
    assert!(attach(&entrances, clone, &cloned_path).is_err());
    assert!(attach(&entrances, original, &path).is_ok());
}

fn attach(entrances: &LocalEntrances, vm: CreationId, handover: &Path) -> Result<(), Error> {
    let socket = SocketDir::new().unwrap();
    let transport = Arc::new(GuestRoute {
        unix: UnixTransport::bind(&socket.address()).unwrap(),
        address: socket.address(),
        vm,
    });
    std::thread::scope(|tasks| {
        let host = transport.clone();
        let server = tasks.spawn(move || {
            let connection = host.accept(ADDRESS)?;
            entrances
                .open(Entrance::General, host.clone(), connection)?
                .attach_participant()?;
            Ok::<_, Error>(())
        });
        let handover = HandoverFile::read(handover).unwrap();
        assert_eq!(handover.address(), ADDRESS);
        let result = LocalClient::participant(transport, handover, Some(Arc::new(Key)));
        let attached = server.join().unwrap();
        assert_eq!(result.is_ok(), attached.is_ok());
        result.map(|_| ())
    })
}

// The socket's VM identity is injected at the same seam used by the verified
// Firecracker route. The protocol and journal below this seam are real.
struct GuestRoute {
    unix: UnixTransport,
    address: String,
    vm: CreationId,
}

impl Transport for GuestRoute {
    type Connection = UnixLink;

    fn describe(&self) -> TransportInfo {
        self.unix.describe()
    }

    fn accept(&self, address: &str) -> Result<Link<UnixLink>, Error> {
        assert_eq!(address, ADDRESS);
        let mut link = self.unix.accept(&self.address)?;
        link.source = Some(Source::VirtualMachine(self.vm));
        Ok(link)
    }

    fn dial(&self, address: &str) -> Result<Link<UnixLink>, Error> {
        assert_eq!(address, ADDRESS);
        self.unix.dial(&self.address)
    }

    fn send(&self, link: &UnixLink, bytes: &[u8]) -> Result<usize, Error> {
        self.unix.send(link, bytes)
    }

    fn receive(&self, link: &UnixLink, output: &mut [u8]) -> Result<Received, Error> {
        self.unix.receive(link, output)
    }
}
