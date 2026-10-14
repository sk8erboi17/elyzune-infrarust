"""mcprotocol.c scenarios for elyzune-via, as a Python unittest.

Builds `via_scenario.c` against mcprotocol.c, starts a loopback fixture

    Infrarust (elyzune-via, offline mode) -> 26.2 backend

and runs the two-bot scenario for every client protocol: 774, 775 and 777
through the translator, 776 as the untranslated control. Each scenario step
is a subtest, so a report names the protocol and the step that failed.

Backends (VIA_BACKEND):

  hyperion  Hyperion's bedwars event behind hyperion-proxy, the Elyzune
            engine; needs HYPERION_BIN (directory with `bedwars` and
            `hyperion-proxy`) and HYPERION_CERTS (root_ca.crt, server.crt,
            server_private_key.pem, proxy.crt, proxy_private_key.pem).
  vanilla   The vanilla 26.2 server jar (VANILLA_JAR), whose richer packet
            stream also runs the scenario's "rich" phase (items, entity data,
            particles, sounds, time, teams, beds). The fixture accepts the
            EULA for this throwaway local world and makes the bots operators.

When the other backend's variables are set too, it is started as a second
server and every scenario also switches the first bot there and back
(`/ir server`), the steps "switch", "switch_back" and "switch_decode".

Always needed: MCPROTOCOL_C (the mcprotocol.c checkout) and INFRARUST_BIN
(the infrarust binary built with the plugin-elyzune-via feature). The test
is skipped when they are not set. Everything listens on 127.0.0.1, on
VIA_PORT_BASE .. +12 (default 41200), and every process is stopped by PID.
Logs (scenario-<protocol>.log has every STEP line) and worlds live in a
fresh directory under VIA_WORK_DIR (default: the system temporary
directory), removed afterwards unless VIA_KEEP_WORK is set.

    MCPROTOCOL_C=... INFRARUST_BIN=... VIA_BACKEND=vanilla VANILLA_JAR=... \\
        python3 -m unittest plugins/elyzune-via/tests/mcprotocol/test_scenarios.py
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import unittest
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROTOCOLS = (776, 774, 775, 777)
STEPS = ("status", "login", "configuration", "spawn", "chunks", "move", "chat", "inventory",
         "see_peer", "decode", "disconnect", "reconnect")
RICH_STEPS = ("rich", "rich_decode", "rich_exact")
SWITCH_STEPS = ("switch", "switch_back", "switch_decode")


def env_path(name: str) -> Path | None:
    value = os.environ.get(name)
    return Path(value) if value else None


def wait_port(port: int, process: subprocess.Popen, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"process exited before opening port {port}")
        with socket.socket() as s:
            s.settimeout(0.5)
            if s.connect_ex(("127.0.0.1", port)) == 0:
                return
        time.sleep(0.5)
    raise RuntimeError(f"port {port} not open after {timeout}s")


def wait_log(path: Path, marker: str, process: subprocess.Popen, timeout: float) -> None:
    """Waits until `marker` appears in the log at `path`."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"process exited before logging {marker!r}")
        if path.is_file() and marker in path.read_text(errors="replace"):
            return
        time.sleep(0.5)
    raise RuntimeError(f"{marker!r} not in {path} after {timeout}s")


def offline_uuid(name: str) -> str:
    digest = bytearray(hashlib.md5(("OfflinePlayer:" + name).encode()).digest())
    digest[6] = (digest[6] & 0x0F) | 0x30
    digest[8] = (digest[8] & 0x3F) | 0x80
    return str(uuid.UUID(bytes=bytes(digest)))


@unittest.skipUnless(os.environ.get("MCPROTOCOL_C") and os.environ.get("INFRARUST_BIN"),
                     "set MCPROTOCOL_C and INFRARUST_BIN to run the mcprotocol.c scenarios")
class ViaScenarios(unittest.TestCase):
    processes: list[subprocess.Popen] = []
    logs: list = []

    @classmethod
    def setUpClass(cls) -> None:
        root = env_path("VIA_WORK_DIR")
        if root is not None:
            root.mkdir(parents=True, exist_ok=True)
        cls.work = Path(tempfile.mkdtemp(prefix="elyzune-via-", dir=root))
        cls.base = int(os.environ.get("VIA_PORT_BASE", "41200"))
        cls.backend = os.environ.get("VIA_BACKEND", "hyperion")
        cls.second = "hyperion" if cls.backend == "vanilla" else "vanilla"
        cls.processes = []
        try:
            cls.scenario = cls.build_scenario()
            backend_port = cls.start_backend(cls.backend, required=True)
            second_port = cls.start_backend(cls.second, required=False)
            cls.switching = second_port is not None
            cls.proxy_port = cls.base + 1
            cls.start_infrarust(backend_port, second_port)
        except Exception:
            cls.stop_all()
            raise

    @classmethod
    def tearDownClass(cls) -> None:
        cls.stop_all()
        if os.environ.get("VIA_KEEP_WORK") is None:
            shutil.rmtree(cls.work, ignore_errors=True)

    @classmethod
    def stop_all(cls) -> None:
        for process in reversed(cls.processes):
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        cls.processes = []
        for log in cls.logs:
            log.close()
        cls.logs = []

    @classmethod
    def spawn(cls, args: list[str], cwd: Path, log: str) -> subprocess.Popen:
        out = open(cls.work / log, "wb")  # noqa: SIM115 - closed in stop_all
        cls.logs.append(out)
        process = subprocess.Popen(args, cwd=cwd, stdout=out, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
        cls.processes.append(process)
        return process

    @classmethod
    def build_scenario(cls) -> Path:
        mcp = env_path("MCPROTOCOL_C")
        assert mcp is not None
        binary = cls.work / "via_scenario"
        subprocess.run(
            ["cc", "-O2", "-std=c11", "-Wno-string-concatenation", f"-I{mcp}", str(HERE / "via_scenario.c"),
             str(mcp / "api.c"), "-lz", "-o", str(binary)],
            check=True,
        )
        return binary

    @classmethod
    def start_backend(cls, kind: str, required: bool) -> int | None:
        """Starts the `kind` backend and returns its port. A backend whose
        variables are missing skips the test when `required`, otherwise it is
        simply not started (None)."""
        if kind == "vanilla":
            jar = env_path("VANILLA_JAR")
            if jar is None:
                if not required:
                    return None
                raise unittest.SkipTest("VIA_BACKEND=vanilla needs VANILLA_JAR")
            port = cls.base + 12
            world = cls.work / "vanilla"
            world.mkdir()
            (world / "eula.txt").write_text("eula=true\n")
            (world / "server.properties").write_text(
                f"server-ip=127.0.0.1\nserver-port={port}\nonline-mode=false\nenforce-secure-profile=false\n"
                "level-type=minecraft\\:flat\ngenerate-structures=false\nspawn-protection=0\nview-distance=4\n"
                "simulation-distance=4\nenable-query=false\nenable-rcon=false\nmanagement-server-enabled=false\n"
                "difficulty=easy\n"
            )
            names = [f"v{who}{p}" for p in PROTOCOLS for who in "AB"]
            (world / "ops.json").write_text(json.dumps(
                [{"uuid": offline_uuid(n), "name": n, "level": 4, "bypassesPlayerLimit": False} for n in names]))
            process = cls.spawn(["java", "-Xmx2G", "-jar", str(jar), "--nogui"], world, "vanilla.log")
            wait_port(port, process, 180)
            # The port opens before the world is ready; a status ping in
            # between did not get the server's 26.2 status (the first
            # protocol's status step once saw another protocol number, the
            # ping a second before "Done"). "Done (" ends the startup.
            wait_log(cls.work / "vanilla.log", "Done (", process, 180)
            return port
        bins = env_path("HYPERION_BIN")
        certs = env_path("HYPERION_CERTS")
        if bins is None or certs is None:
            if not required:
                return None
            raise unittest.SkipTest("VIA_BACKEND=hyperion needs HYPERION_BIN and HYPERION_CERTS")
        game, player = cls.base + 10, cls.base + 11
        run = cls.work / "hyperion"
        run.mkdir()
        server = cls.spawn(
            [str(bins / "bedwars"), "--ip", "127.0.0.1", "--port", str(game), "--root-ca-cert", str(certs / "root_ca.crt"),
             "--cert", str(certs / "server.crt"), "--private-key", str(certs / "server_private_key.pem")],
            run, "bedwars.log")
        wait_port(game, server, 180)
        proxy = cls.spawn(
            [str(bins / "hyperion-proxy"), "--server", f"127.0.0.1:{game}", "--root-ca-cert", str(certs / "root_ca.crt"),
             "--cert", str(certs / "proxy.crt"), "--private-key", str(certs / "proxy_private_key.pem"), f"127.0.0.1:{player}"],
            run, "hyperion-proxy.log")
        wait_port(player, proxy, 60)
        return player

    @classmethod
    def start_infrarust(cls, backend_port: int, second_port: int | None) -> None:
        binary = env_path("INFRARUST_BIN")
        assert binary is not None
        home = cls.work / "infrarust"
        (home / "servers").mkdir(parents=True)
        (home / "infrarust.toml").write_text(
            f'bind = "127.0.0.1:{cls.proxy_port}"\nservers_dir = "./servers"\nplugins_dir = "./plugins"\n'
            '\n[permissions]\nplayer_commands = ["server"]\n')
        (home / "servers" / "elyzune.toml").write_text(
            f'domains = ["127.0.0.1"]\naddresses = ["127.0.0.1:{backend_port}"]\nproxy_mode = "offline"\n')
        if second_port is not None:
            (home / "servers" / "second.toml").write_text(
                f'domains = ["second.localhost"]\naddresses = ["127.0.0.1:{second_port}"]\nproxy_mode = "offline"\n')
        process = cls.spawn([str(binary), "--config", "infrarust.toml"], home, "infrarust.log")
        wait_port(cls.proxy_port, process, 60)

    def names_file(self, protocol: int) -> Path:
        """The client release's slot displays, recipe displays and argument
        parsers in id order, from Mojang's reports (via_data.py output in
        VIA_DATA, default <infrarust>/target/via-data)."""
        data = env_path("VIA_DATA") or HERE.parents[3] / "target" / "via-data"
        release = {774: "1.21.11", 775: "26.1.2", 776: "26.2", 777: "26.3"}[protocol]
        reports = json.loads((data / "gen" / release / "generated" / "reports" / "registries.json").read_text())
        lines = []
        for registry in ("minecraft:slot_display", "minecraft:recipe_display", "minecraft:command_argument_type"):
            entries = reports[registry]["entries"]
            lines.append(",".join(sorted(entries, key=lambda name: entries[name]["protocol_id"])))
        path = self.work / f"names-{protocol}.txt"
        path.write_text("\n".join(lines) + "\n")
        return path

    def run_protocol(self, protocol: int) -> None:
        rich = self.backend == "vanilla"
        args = [str(self.scenario), str(protocol), "127.0.0.1", str(self.proxy_port), "v"]
        if rich:
            args += ["rich", f"names:{self.names_file(protocol)}"]
        if self.switching:
            args.append("switch:second:elyzune")
        result = subprocess.run(args, capture_output=True, text=True, timeout=900)
        (self.work / f"scenario-{protocol}.log").write_text(result.stdout + result.stderr)
        steps = {}
        for line in result.stdout.splitlines():
            parts = line.split(" ", 4)
            if parts[0] == "STEP":
                steps[parts[2]] = (parts[3], parts[4] if len(parts) > 4 else "")
        expected = STEPS + (RICH_STEPS if rich else ()) + (SWITCH_STEPS if self.switching else ())
        for name in expected:
            with self.subTest(protocol=protocol, step=name):
                self.assertIn(name, steps, f"step {name} did not run:\n{result.stdout}\n{result.stderr}")
                status, detail = steps[name]
                self.assertEqual(status, "PASS", f"{protocol} {name}: {detail}")
        log = (self.work / "infrarust.log").read_text(errors="replace")
        with self.subTest(protocol=protocol, step="translator_log"):
            self.assertNotIn("could not translate the packet body", log)

    def test_776_control(self) -> None:
        self.run_protocol(776)

    def test_774_through_translator(self) -> None:
        self.run_protocol(774)

    def test_775_through_translator(self) -> None:
        self.run_protocol(775)

    def test_777_through_translator(self) -> None:
        self.run_protocol(777)


if __name__ == "__main__":
    unittest.main()
