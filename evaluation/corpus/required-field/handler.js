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
  if (request.method === "POST" && request.path === "/users") {
    const input = JSON.parse((request.bodyText ?? decodeUtf8(request.body)));
    if (typeof input.name !== "string" || !input.name.trim()) {
      return text(400, "name is required", [header("content-type", "text/plain")]);
    }
    return text(201, "created");
  }
  return text(404, "not found");
}
