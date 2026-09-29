python3 - <<'SEEDANCE_REFERENCE_PY'
import base64, json, os, stat, sys, urllib.request
cfg = json.loads(base64.b64decode('__CONFIG_BASE64__'))
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None
try:
    opener = urllib.request.build_opener(NoRedirect)
    for index, path in enumerate(cfg['paths']):
        info = os.lstat(path)
        if not stat.S_ISREG(info.st_mode) or not 8 <= info.st_size <= 33554432:
            raise ValueError('invalid media file')
        with open(path, 'rb') as source:
            data = source.read(33554433)
        if len(data) > 33554432:
            raise ValueError('media too large')
        request = urllib.request.Request(cfg['base'] + '/' + str(index), data=data, method='POST',
            headers={'X-Seedance-Upload': cfg['authorization'], 'Content-Type': 'application/octet-stream'})
        with opener.open(request, timeout=120) as result:
            if result.status != 200:
                raise ValueError('upload rejected')
    print('SEEDANCE_REFERENCE_UPLOAD=' + json.dumps({'id':cfg['id'], 'status':'uploaded', 'files':len(cfg['paths'])}))
except Exception:
    print('SEEDANCE_REFERENCE_UPLOAD_FAILED: media unavailable, network denied, or upload rejected. No video was submitted.')
    sys.exit(1)
SEEDANCE_REFERENCE_PY
