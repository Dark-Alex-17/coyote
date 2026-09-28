#!/usr/bin/env python3
"""The pinned Python Reticulum/LXMF reference as one peer, driven over stdin/stdout.

One JSON object per line. Commands come in as ``{"id": N, "cmd": "...", ...}`` and are
answered with ``{"id": N, "ok": true, ...}`` or ``{"id": N, "ok": false, "error": "..."}``;
anything the reference observes on its own is an unsolicited ``{"event": "...", ...}``
line. Bytes travel as lowercase hex. Log noise goes to stderr only.

On start the process writes a Reticulum config with transport enabled and a single
TCPServerInterface on a free loopback port, starts Reticulum, creates a Coyote-shaped peer
destination (``coyote.mesh.<instance_id>``) serving ``/status`` and ``/message``, and prints
``READY {json}`` once the Rust side may connect.
"""

import json
import os
import secrets
import shutil
import socket
import sys
import tempfile
import threading
import time
import traceback

import RNS
import LXMF
from LXMF.LXStamper import STAMP_SIZE

MESH_APP = "coyote"
ANNOUNCE_MAGIC = b"COYM"
PROTOCOL_VERSION = 1
VERSION_OFFSET = len(ANNOUNCE_MAGIC)
NAME_OFFSET = VERSION_OFFSET + 2
NAME_HASH_LEN = 10

_stdout_lock = threading.Lock()


def log(message):
    print(f"[reference_peer] {message}", file=sys.stderr, flush=True)


def emit(obj):
    with _stdout_lock:
        print(json.dumps(obj), flush=True)


def jsonable(value):
    if isinstance(value, bytes):
        return value.hex()
    if isinstance(value, dict):
        return {str(jsonable(k)): jsonable(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [jsonable(v) for v in value]
    return value


def mesh_aspects(instance_id):
    """`coyote.mesh.<instance_id>` as Reticulum wants it: one aspect per dotted segment."""
    return ("mesh", instance_id)


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def write_config(config_dir, port):
    with open(os.path.join(config_dir, "config"), "w") as handle:
        handle.write(
            "[reticulum]\n"
            "  enable_transport = Yes\n"
            "  share_instance = No\n"
            "  respond_to_probes = No\n"
            "\n"
            "[interfaces]\n"
            "  [[Relay]]\n"
            "    type = TCPServerInterface\n"
            "    enabled = Yes\n"
            "    listen_ip = 127.0.0.1\n"
            f"    listen_port = {port}\n"
        )


class AnnounceWatch:
    def __init__(self, instance_id):
        self.aspects = mesh_aspects(instance_id)
        self.aspect_filter = ".".join((MESH_APP,) + self.aspects)

    def received_announce(self, destination_hash, announced_identity, app_data):
        app_data = app_data or b""
        magic_ok = app_data[:VERSION_OFFSET] == ANNOUNCE_MAGIC
        version = (
            int.from_bytes(app_data[VERSION_OFFSET:NAME_OFFSET], "big")
            if len(app_data) >= NAME_OFFSET
            else None
        )
        name = (
            app_data[NAME_OFFSET:].decode("utf-8", errors="replace")
            if len(app_data) > NAME_OFFSET
            else None
        )
        emit(
            {
                "event": "announce",
                "destination_hash": destination_hash.hex(),
                "identity_hash": announced_identity.hash.hex(),
                "app_data": app_data.hex(),
                "derived_destination_hash": RNS.Destination.hash(
                    announced_identity, MESH_APP, *self.aspects
                ).hex(),
                "decoded": {"magic_ok": magic_ok, "version": version, "display_name": name},
            }
        )


class ReferencePeer:
    def __init__(self):
        self.tmp = tempfile.mkdtemp(prefix="coyote-mesh-reference-")
        try:
            self._start()
        except Exception:
            shutil.rmtree(self.tmp, ignore_errors=True)
            raise

    def _start(self):
        self.relay_port = free_port()
        config_dir = os.path.join(self.tmp, "reticulum")
        os.makedirs(config_dir)
        write_config(config_dir, self.relay_port)
        level = RNS.LOG_DEBUG if os.environ.get("COYOTE_MESH_INTEROP_DEBUG") else RNS.LOG_CRITICAL
        self.reticulum = RNS.Reticulum(configdir=config_dir, loglevel=level, logdest=log)

        self.identity = RNS.Identity()
        self.instance_id = secrets.token_hex(16)
        aspects = mesh_aspects(self.instance_id)
        self.destination = RNS.Destination(
            self.identity, RNS.Destination.IN, RNS.Destination.SINGLE, MESH_APP, *aspects
        )
        expected_name_hash = RNS.Identity.full_hash(
            ".".join((MESH_APP,) + aspects).encode("utf-8")
        )[:NAME_HASH_LEN]
        if self.destination.name_hash != expected_name_hash:
            raise RuntimeError("the destination's name hash is not trunc_10(SHA-256(full name))")
        self.destination.register_request_handler(
            "/status", response_generator=self.serve_status, allow=RNS.Destination.ALLOW_ALL
        )
        self.destination.register_request_handler(
            "/message", response_generator=self.serve_message, allow=RNS.Destination.ALLOW_ALL
        )
        self.watches = []
        self.router = None
        self.router_identity = None

    def ready(self):
        return {
            "relay_port": self.relay_port,
            "identity_hash": self.identity.hash.hex(),
            "destination_hash": self.destination.hash.hex(),
            "name_hash": self.destination.name_hash.hex(),
            "instance_id": self.instance_id,
        }

    # Request handlers on the peer destination.

    def _note_request(self, path, data, remote_identity):
        emit(
            {
                "event": "request",
                "path": path,
                "remote_identity": remote_identity.hash.hex() if remote_identity else None,
                "envelope": jsonable(data),
            }
        )

    def serve_status(self, path, data, request_id, link_id, remote_identity, requested_at):
        self._note_request(path, data, remote_identity)
        return {"v": 1, "state": {"code": 1}, "served_at_secs": int(time.time())}

    def serve_message(self, path, data, request_id, link_id, remote_identity, requested_at):
        self._note_request(path, data, remote_identity)
        body = data.get("body") if isinstance(data, dict) else None
        message_id = body.get("id") if isinstance(body, dict) else None
        return {"received": True, "id": message_id}

    # Commands.

    def cmd_announce(self, args):
        name = args.get("display_name")
        app_data = ANNOUNCE_MAGIC + PROTOCOL_VERSION.to_bytes(2, "big")
        if name is not None:
            app_data += name.encode("utf-8")
        self.destination.announce(app_data=app_data)
        return {"app_data": app_data.hex()}

    def cmd_watch(self, args):
        watch = AnnounceWatch(args["instance_id"])
        RNS.Transport.register_announce_handler(watch)
        self.watches.append(watch)
        return {"aspect_filter": watch.aspect_filter}

    def cmd_wait_path(self, args):
        destination_hash = bytes.fromhex(args["destination_hash"])
        deadline = time.time() + float(args.get("timeout_secs", 15))
        if not RNS.Transport.has_path(destination_hash):
            RNS.Transport.request_path(destination_hash)
        while time.time() < deadline:
            if RNS.Transport.has_path(destination_hash) and RNS.Identity.recall(destination_hash):
                return {"hops": RNS.Transport.hops_to(destination_hash)}
            time.sleep(0.05)
        raise TimeoutError(f"no path to {destination_hash.hex()}")

    def _envelope_from(self, args):
        if "raw_envelope" in args:
            return args["raw_envelope"]
        envelope = {}
        for key, value in args["envelope"].items():
            if key == "name_hash" and isinstance(value, str):
                value = bytes.fromhex(value)
            envelope[key] = value
        return envelope

    def cmd_request(self, args):
        destination_hash = bytes.fromhex(args["destination_hash"])
        timeout = float(args.get("timeout_secs", 20))
        identity = RNS.Identity.recall(destination_hash)
        if identity is None:
            raise RuntimeError(f"identity of {destination_hash.hex()} is unknown; wait_path first")
        out = RNS.Destination(
            identity,
            RNS.Destination.OUT,
            RNS.Destination.SINGLE,
            MESH_APP,
            *mesh_aspects(args["instance_id"]),
        )
        if out.hash != destination_hash:
            raise RuntimeError(
                f"{destination_hash.hex()} is not coyote.mesh.{args['instance_id']} of the recalled identity"
            )
        established = threading.Event()
        link = RNS.Link(out, established_callback=lambda _link: established.set())
        if not established.wait(timeout):
            link.teardown()
            raise TimeoutError("link did not become active")
        link.identify(self.identity)
        done = threading.Event()
        receipt = link.request(
            args["path"],
            data=self._envelope_from(args),
            response_callback=lambda _receipt: done.set(),
            failed_callback=lambda _receipt: done.set(),
            timeout=timeout,
        )
        if receipt is False:
            link.teardown()
            raise RuntimeError("the request was not sent")
        done.wait(timeout + 5)
        status = receipt.get_status()
        response = receipt.get_response() if status == RNS.RequestReceipt.READY else None
        link.teardown()
        return {
            "status": "ready" if status == RNS.RequestReceipt.READY else "failed",
            "response": jsonable(response),
            "response_type": type(response).__name__.lower() if response is not None else "none",
        }

    def cmd_silence(self, args):
        if not self.destination.deregister_request_handler(args["path"]):
            raise ValueError(f"no handler registered at {args['path']!r}")
        return {}

    def cmd_pn_start(self, args):
        if self.router is not None:
            raise RuntimeError("the propagation node is already running")
        self.router_identity = RNS.Identity()
        storage = os.path.join(self.tmp, "lxmf")
        os.makedirs(storage)
        self.router = LXMF.LXMRouter(
            identity=self.router_identity,
            storagepath=storage,
            autopeer=False,
            propagation_cost=int(args["cost"]),
        )
        self.router.enable_propagation()
        # `enable_propagation` schedules the node announce `NODE_ANNOUNCE_DELAY` (20 s) out;
        # the same announce is sent now so the test does not wait for it.
        self.router.propagation_destination.announce(
            app_data=self.router.get_propagation_node_app_data()
        )
        return {
            "destination_hash": self.router.propagation_destination.hash.hex(),
            "stamp_cost": self.router.propagation_stamp_cost,
            "stamp_cost_flexibility": self.router.propagation_stamp_cost_flexibility,
        }

    def cmd_pn_count(self, args):
        if self.router is None:
            raise RuntimeError("no propagation node is running")
        return {"count": len(self.router.propagation_entries)}

    def cmd_pn_messages(self, args):
        """Every stored message addressed to this peer's `lxmf.delivery`, decrypted and
        unpacked with the peer identity, which is what its recipient would do after a fetch."""
        if self.router is None:
            raise RuntimeError("no propagation node is running")
        # OUT so Transport does not register it (an IN destination could only be created
        # once); decrypting needs nothing beyond the private identity.
        delivery = RNS.Destination(
            self.identity, RNS.Destination.OUT, RNS.Destination.SINGLE, LXMF.APP_NAME, "delivery"
        )
        messages = []
        for transient_id, entry in list(self.router.propagation_entries.items()):
            with open(entry[1], "rb") as handle:
                stamped = handle.read()
            lxmf_data = stamped[:-STAMP_SIZE]
            destination_hash = lxmf_data[: LXMF.LXMessage.DESTINATION_LENGTH]
            record = {
                "transient_id": transient_id.hex(),
                "destination_hash": destination_hash.hex(),
                "stamp_value": entry[6],
            }
            if destination_hash == delivery.hash:
                plaintext = delivery.decrypt(lxmf_data[LXMF.LXMessage.DESTINATION_LENGTH :])
                if plaintext is not None:
                    message = LXMF.LXMessage.unpack_from_bytes(destination_hash + plaintext)
                    record.update(
                        {
                            "source_hash": message.source_hash.hex(),
                            "title": message.title_as_string(),
                            "content": message.content_as_string(),
                            "fields": jsonable(message.fields),
                            "timestamp": message.timestamp,
                        }
                    )
            messages.append(record)
        return {"messages": messages}

    def cmd_quit(self, args):
        return {}

    def handle(self, command):
        handler = getattr(self, f"cmd_{command['cmd']}", None)
        if handler is None:
            raise ValueError(f"unknown command {command['cmd']!r}")
        return handler(command)

    def shutdown(self):
        try:
            if self.router is not None:
                self.router.exit_handler()
            RNS.Reticulum.exit_handler()
        finally:
            shutil.rmtree(self.tmp, ignore_errors=True)


def serve(peer):
    """Answers commands until `quit` or EOF. A line that is not a JSON object is answered
    with `id: null` and does not stop the loop."""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        command = None
        reply = {"id": None}
        try:
            command = json.loads(line)
            if not isinstance(command, dict):
                raise ValueError("a command must be a JSON object")
            reply["id"] = command.get("id")
            reply.update(peer.handle(command))
            reply["ok"] = True
        except Exception as err:
            what = command.get("cmd") if isinstance(command, dict) else line
            log(f"{what} failed: {err!r}")
            reply.update({"ok": False, "error": f"{type(err).__name__}: {err}"})
        emit(reply)
        if isinstance(command, dict) and command.get("cmd") == "quit":
            return


def main():
    peer = None
    exit_code = 0
    try:
        peer = ReferencePeer()
        print("READY " + json.dumps(peer.ready()), flush=True)
        serve(peer)
    except Exception:
        log(traceback.format_exc())
        exit_code = 1
    finally:
        if peer is not None:
            try:
                peer.shutdown()
            except Exception:
                log(traceback.format_exc())
                exit_code = 1
        # Reticulum leaves non-daemon threads behind; nothing here needs to outlive the
        # command stream.
        os._exit(exit_code)


if __name__ == "__main__":
    main()
