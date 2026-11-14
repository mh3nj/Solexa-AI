import http.server, urllib.request, json, sys
LOG='/tmp/proxy_requests.jsonl'
class H(http.server.BaseHTTPRequestHandler):
    protocol_version='HTTP/1.0'
    def _fwd(self):
        n=int(self.headers.get('Content-Length') or 0)
        body=self.rfile.read(n) if n else None
        if self.command=='POST' and body:
            with open(LOG,'a') as f: f.write(json.dumps({'path':self.path,'body':json.loads(body)})+'\n')
        req=urllib.request.Request('http://127.0.0.1:8080'+self.path,data=body,method=self.command,headers={k:v for k,v in self.headers.items() if k.lower() not in('host','content-length')})
        try: r=urllib.request.urlopen(req)
        except urllib.error.HTTPError as e: r=e
        self.send_response(r.status)
        for k,v in r.getheaders():
            if k.lower() not in('transfer-encoding','connection'): self.send_header(k,v)
        self.end_headers()
        while True:
            c=r.read(4096)
            if not c: break
            self.wfile.write(c); self.wfile.flush()
    do_GET=do_POST=_fwd
    def log_message(self,*a): pass
http.server.ThreadingHTTPServer(('127.0.0.1',8090),H).serve_forever()
