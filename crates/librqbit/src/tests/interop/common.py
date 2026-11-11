# Prepended to every interop test script, see mod.rs.
#
# A script reports events with emit() and takes commands with next_command(). Everything it
# writes to stderr shows up in the output of the test.

import json
import os
import queue
import sys
import threading
import time
import traceback

import libtorrent as lt

_commands = queue.Queue()


def _read_commands():
    for line in sys.stdin:
        line = line.strip()
        if line:
            _commands.put(json.loads(line))
    # The test went away.
    _commands.put({"cmd": "quit"})


threading.Thread(target=_read_commands, daemon=True).start()


def emit(event, **fields):
    """Reports an event to the test."""
    fields["event"] = event
    sys.stdout.write(json.dumps(fields) + "\n")
    sys.stdout.flush()


def next_command(timeout=None):
    """The next command of the test, or None when there is none within `timeout` seconds.

    {"cmd": "quit"} is returned once the test closed its end.
    """
    try:
        return _commands.get(timeout=timeout)
    except queue.Empty:
        return None


def log(*args):
    print(*args, file=sys.stderr, flush=True)


def make_session(**overrides):
    """A session on loopback that finds peers only when it is told about them."""
    settings = {
        "listen_interfaces": "127.0.0.1:0",
        "enable_dht": False,
        "enable_lsd": False,
        "enable_upnp": False,
        "enable_natpmp": False,
        # Every peer of a test is 127.0.0.1.
        "allow_multiple_connections_per_ip": True,
        "alert_mask": lt.alert_category.all,
    }
    settings.update(overrides)
    return lt.session(settings)


def started_params(info, save_path):
    """Parameters to add the torrent `info` with, so that it starts right away.

    By default a torrent is added paused and left to the queueing logic of libtorrent, which
    takes its time.
    """
    params = lt.add_torrent_params()
    params.ti = info
    params.save_path = save_path
    params.flags &= ~(lt.torrent_flags.auto_managed | lt.torrent_flags.paused)
    return params


def wait_listen_port(session, timeout=10):
    """The TCP port the session listens on."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        port = session.listen_port()
        if port:
            return port
        time.sleep(0.05)
    raise RuntimeError("libtorrent did not start listening")


def pop_alerts(session, wait=0.2):
    """Waits up to `wait` seconds for alerts and returns the ones that are queued."""
    session.wait_for_alert(int(wait * 1000))
    return session.pop_alerts()


def save_path_for(info, content_dir):
    """The libtorrent save path under which the content in `content_dir` is found.

    rqbit keeps the files of a torrent directly in its output folder, libtorrent keeps the
    files of a multi-file torrent in a folder named after the torrent.
    """
    content_dir = os.path.abspath(content_dir)
    if info.num_files() > 1 or os.sep in info.layout().file_path(0):
        return os.path.dirname(content_dir)
    return content_dir


def first_file_path(info, save_path="."):
    """Where libtorrent keeps the first file of the torrent."""
    return os.path.abspath(os.path.join(save_path, info.layout().file_path(0)))


def run(main):
    """Runs the script, and reports an exception as an "error" event."""
    try:
        main()
    except Exception as e:  # noqa: BLE001
        traceback.print_exc()
        emit("error", message=repr(e))
        sys.exit(1)
