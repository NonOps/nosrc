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

export function handle(request) {
  const match = request.path.match(/^\/items\/([^/]+)$/);
  if (request.method === "GET" && match) {
    return text(200, JSON.stringify({ id: match[1] }), [header("content-type", "application/json")]);
  }
  return text(404, "not found");
}
