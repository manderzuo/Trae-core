"""Run the production POSIX downloader on Linux, using only owned test files."""
import argparse, http.server, json, pathlib, subprocess, tempfile, threading

args = argparse.ArgumentParser()
args.add_argument('--temp-root', required=True)
options = args.parse_args()
payload = b'\0\0\0\x18ftypisom\0\0\0\0isommp42'
scenario = {'mime': 'video/mp4', 'short': False}

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Type', scenario['mime'])
        self.send_header('Content-Length', str(len(payload) + (12 if scenario['short'] else 0)))
        self.end_headers()
        self.wfile.write(payload)
    def log_message(self, *args):
        pass

source = pathlib.Path(__file__).resolve().parents[1] / 'starlink-dimension-router/src/video_download.sh'
with tempfile.TemporaryDirectory(prefix='posix-delivery-', dir=options.temp_root) as directory:
    home = pathlib.Path(directory)
    destination = home / "owner's Downloads"
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    def quote(value):
        return "'" + str(value).replace("'", "'\"'\"'") + "'"
    try:
        results = []
        for mime, short in [('video/mp4', False), ('video/mp4', False), ('text/html', False), ('video/mp4', True)]:
            scenario.update(mime=mime, short=short)
            script = source.read_text().replace('__REQUEST__', quote('request_posix'))
            script = script.replace('__URL__', quote(f'http://127.0.0.1:{server.server_port}/video'))
            script = script.replace('__DIRECTORY__', quote(''))
            # Emulate an OS-known Downloads directory without changing HOME,
            # user configuration, or the production downloader.
            script = "xdg-user-dir() { printf '%s\\n' " + quote(destination) + "; }\n" + script
            run = subprocess.run(['bash', '-c', script], capture_output=True, text=True, timeout=15)
            receipt = json.loads(next(line.split('=', 1)[1] for line in run.stdout.splitlines() if line.startswith('SEEDANCE_DELIVERY_RECEIPT=')))
            success = mime == 'video/mp4' and not short
            assert (run.returncode == 0) == success, run.stderr
            assert receipt['status'] == ('saved' if success else 'failed')
            for path in destination.iterdir():
                assert path.suffix == '.mp4' and path.read_bytes() == payload, 'no partial or corrupted files'
            results.append(receipt['status'])
        assert len(list(destination.iterdir())) == 2, 'retry must not overwrite'
        print(json.dumps({'posix_scenarios': results, 'cleanup': 'TemporaryDirectory'}))
    finally:
        server.shutdown()
