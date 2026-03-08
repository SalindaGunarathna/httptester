const DB_NAME = 'webhookpush';
const DB_VERSION = 1;
const REQUESTS_STORE = 'requests';
const PENDING_STORE = 'pending_chunks';
const PENDING_TTL_MS = 30000;

self.addEventListener('install', (event) => {
  event.waitUntil(self.skipWaiting());
});

self.addEventListener('activate', (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener('push', (event) => {
  event.waitUntil(handlePush(event));
});

self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil(focusClient());
});

async function handlePush(event) {
  if (!event.data) return;

  let envelope;
  try {
    envelope = event.data.json();
  } catch {
    try {
      envelope = JSON.parse(event.data.text());
    } catch {
      return;
    }
  }

  if (
    !envelope ||
    !envelope.request_id ||
    !envelope.data ||
    !envelope.chunk_index
  ) {
    return;
  }

  const db = await openDb();
  await storeChunk(db, envelope);

  const result = await tryAssemble(db, envelope.request_id);
  if (result?.request) {
    await notifyClients(result.request.id, result.partial);
    await showSummary(result.request, result.partial);
  }
}

async function tryAssemble(db, requestId) {
  const chunks = await getChunksForRequest(db, requestId);
  if (!chunks.length) return null;

  const lastChunk =
    chunks.find((chunk) => chunk.is_last) ||
    chunks.find(
      (chunk) =>
        chunk.total_chunks && chunk.chunk_index === chunk.total_chunks
    );
  const totalChunks =
    (lastChunk && (lastChunk.total_chunks || lastChunk.chunk_index)) ||
    chunks[0].total_chunks;
  if (totalChunks && chunks.length === totalChunks) {
    chunks.sort((a, b) => a.chunk_index - b.chunk_index);
    const bytes = concatChunks(chunks);
    const payload = parsePayload(bytes, requestId);
    payload.received_at = Date.now();
    payload.partial = false;
    await storeRequest(db, payload);
    await deleteChunks(db, chunks);
    return { request: payload, partial: false };
  }

  const oldest = chunks.reduce(
    (min, chunk) => Math.min(min, chunk.received_at),
    chunks[0].received_at
  );
  if (Date.now() - oldest > PENDING_TTL_MS) {
    const missing = totalChunks ? totalChunks - chunks.length : null;
    const payload = {
      id: requestId,
      timestamp: new Date().toISOString(),
      method: 'PARTIAL',
      path: '/',
      query_string: '',
      headers: {},
      body: '',
      source_ip: '',
      content_length: 0,
      partial: true,
      missing_chunks: missing,
      note: missing
        ? `Partial delivery: missing ${missing} chunk(s).`
        : 'Partial delivery: missing chunks.',
      received_at: Date.now(),
    };
    await storeRequest(db, payload);
    await deleteChunks(db, chunks);
    return { request: payload, partial: true };
  }

  return null;
}

function parsePayload(bytes, requestId) {
  if (bytes.length >= 8) {
    const magic = String.fromCharCode(
      bytes[0],
      bytes[1],
      bytes[2],
      bytes[3]
    );
    if (magic === 'WHP1') {
      const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      const metaLen = view.getUint32(4);
      if (bytes.length >= 8 + metaLen) {
        const metaBytes = bytes.slice(8, 8 + metaLen);
        const bodyBytes = bytes.slice(8 + metaLen);
        let meta = {};
        try {
          meta = JSON.parse(new TextDecoder().decode(metaBytes));
        } catch {
          meta = {};
        }
        const bodyText = new TextDecoder().decode(bodyBytes);
        return {
          id: requestId,
          timestamp: meta.timestamp || new Date().toISOString(),
          method: meta.method || 'UNKNOWN',
          path: meta.path || '/',
          query_string: meta.query_string || '',
          headers: meta.headers || {},
          body: bodyText,
          source_ip: meta.source_ip || '',
          content_length: bodyBytes.length,
        };
      }
    }
  }

  const payloadText = new TextDecoder().decode(bytes);
  try {
    return JSON.parse(payloadText);
  } catch {
    return {
      id: requestId,
      timestamp: new Date().toISOString(),
      method: 'UNKNOWN',
      path: '/',
      query_string: '',
      headers: {},
      body: payloadText,
      source_ip: '',
      content_length: bytes.length,
    };
  }
}

async function showSummary(request, partial) {
  const title = partial ? 'Partial webhook received' : 'Webhook received';
  const body = partial
    ? request.note || 'Some chunks did not arrive.'
    : `${request.method} ${request.path || ''}`;
  await self.registration.showNotification(title, {
    body,
    tag: request.id,
  });
}

async function notifyClients(id, partial) {
  const clients = await self.clients.matchAll({
    includeUncontrolled: true,
    type: 'window',
  });
  for (const client of clients) {
    client.postMessage({ type: 'new-request', id, partial });
  }
}

async function focusClient() {
  const clients = await self.clients.matchAll({
    type: 'window',
    includeUncontrolled: true,
  });
  if (clients.length) {
    return clients[0].focus();
  }
  return self.clients.openWindow('/');
}

function storeChunk(db, envelope) {
  const hasTotal =
    Number.isInteger(envelope.total_chunks) && envelope.total_chunks > 0;
  const isLast =
    envelope.is_last ||
    (hasTotal && envelope.chunk_index === envelope.total_chunks);
  const record = {
    request_id: envelope.request_id,
    chunk_index: envelope.chunk_index,
    total_chunks: hasTotal ? envelope.total_chunks : null,
    is_last: Boolean(isLast),
    data: envelope.data,
    received_at: Date.now(),
  };
  return new Promise((resolve, reject) => {
    const tx = db.transaction(PENDING_STORE, 'readwrite');
    tx.objectStore(PENDING_STORE).put(record);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

function storeRequest(db, payload) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(REQUESTS_STORE, 'readwrite');
    tx.objectStore(REQUESTS_STORE).put(payload);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

function deleteChunks(db, chunks) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(PENDING_STORE, 'readwrite');
    const store = tx.objectStore(PENDING_STORE);
    chunks.forEach((chunk) => {
      store.delete([chunk.request_id, chunk.chunk_index]);
    });
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

function getChunksForRequest(db, requestId) {
  return new Promise((resolve, reject) => {
    const tx = db.transaction(PENDING_STORE, 'readonly');
    const index = tx.objectStore(PENDING_STORE).index('request_id');
    const req = index.getAll(requestId);
    req.onsuccess = () => resolve(req.result || []);
    req.onerror = () => reject(req.error);
  });
}

function concatChunks(chunks) {
  const decoded = chunks.map((chunk) => base64ToBytes(chunk.data));
  const totalLength = decoded.reduce((sum, arr) => sum + arr.length, 0);
  const output = new Uint8Array(totalLength);
  let offset = 0;
  decoded.forEach((arr) => {
    output.set(arr, offset);
    offset += arr.length;
  });
  return output;
}

function base64ToBytes(base64) {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

function openDb() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(REQUESTS_STORE)) {
        const store = db.createObjectStore(REQUESTS_STORE, { keyPath: 'id' });
        store.createIndex('timestamp', 'timestamp', { unique: false });
        store.createIndex('method', 'method', { unique: false });
      }
      if (!db.objectStoreNames.contains(PENDING_STORE)) {
        const store = db.createObjectStore(PENDING_STORE, {
          keyPath: ['request_id', 'chunk_index'],
        });
        store.createIndex('request_id', 'request_id', { unique: false });
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}
