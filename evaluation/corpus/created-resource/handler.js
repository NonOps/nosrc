function decodeUtf8(bytes) {
  let encoded = "";
  for (const byte of bytes) encoded += "%" + byte.toString(16).padStart(2, "0");
  return decodeURIComponent(encoded);
}

function encodeUtf8(value) {
  const encoded = encodeURIComponent(value);
  const bytes = [];
  for (let i = 0; i < encoded.length; i++) {
    if (encoded[i] === "%") {
      bytes.push(parseInt(encoded.slice(i + 1, i + 3), 16));
      i += 2;
    } else {
      bytes.push(encoded.charCodeAt(i));
    }
  }
  return new Uint8Array(bytes);
}

function header(name, value) {
  return { name, value: encodeUtf8(value) };
}

function text(status, body, headers = []) {
  return { status, headers, body: { tag: "text", val: body } };
}

const items = [];

export function handle(request) {
  if (request.method === "POST" && request.path === "/items") {
    const input = JSON.parse((request.bodyText ?? decodeUtf8(request.body)));
    const id = String(items.length + 1);
    const item = { id, name: String(input.name || "") };
    items.push(item);
    return text(201, JSON.stringify({ id: item.id, name: item.name }), [header("content-type", "application/json"), header("location", `/items/${id}`)]);
  }
  const match = request.path.match(/^\/items\/([^/]+)$/);
  if (request.method === "GET" && match) {
    const item = items.find(candidate => candidate.id === match[1]);
    if (!item) return text(404, "not found");
    return text(200, JSON.stringify(item), [header("content-type", "application/json")]);
  }
  return text(404, "not found");
}
