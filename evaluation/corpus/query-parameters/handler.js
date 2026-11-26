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

function queryValue(query, wanted) {
  if (query === null) return "";
  for (const part of query.split("&")) {
    const pair = part.split("=");
    if (decodeURIComponent((pair[0] || "").replace(/\+/g, " ")) === wanted) {
      return decodeURIComponent((pair.slice(1).join("=") || "").replace(/\+/g, " "));
    }
  }
  return "";
}

export function handle(request) {
  if (request.method === "GET" && request.path === "/search") {
    return text(200, JSON.stringify({ term: queryValue(request.query, "q") }), [header("content-type", "application/json")]);
  }
  return text(404, "not found");
}
