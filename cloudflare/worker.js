const UUID_PATH = /^\/[0-9a-f]{12}$/i;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const path = url.pathname;

    if (isBackendPath(path)) {
      return proxyTo(request, env.BACKEND_ORIGIN);
    }

    return serveAsset(request, env);
  },
};

function isBackendPath(path) {
  if (path === '/health') return true;
  if (path === '/api' || path.startsWith('/api/')) return true;
  if (path === '/hook' || path.startsWith('/hook/')) return true;
  if (UUID_PATH.test(path)) return true;
  return false;
}

function proxyTo(request, origin) {
  const targetUrl = new URL(request.url);
  const originUrl = new URL(origin);
  targetUrl.protocol = originUrl.protocol;
  targetUrl.host = originUrl.host;

  const proxiedRequest = new Request(targetUrl.toString(), request);
  return fetch(proxiedRequest);
}

async function serveAsset(request, env) {
  const url = new URL(request.url);

  let response = await env.ASSETS.fetch(new Request(url.toString(), request));
  if (response.status === 404 && url.pathname !== '/index.html') {
    const indexUrl = new URL(request.url);
    indexUrl.pathname = '/index.html';
    response = await env.ASSETS.fetch(new Request(indexUrl.toString(), request));
  }
  return response;
}
