function parseAndSerializeJson(text) {
    const value = JSON.parse(text);
    return JSON.stringify(value);
}

export function handleRecord(body) {
    if (!(body.bytes instanceof Uint8Array)) {
        throw new TypeError("list<u8> did not map to Uint8Array");
    }
    if (typeof body.utf8Text !== "string") {
        throw new TypeError("record body text field did not map to a string");
    }
    return parseAndSerializeJson(body.utf8Text);
}

export function handleVariant(body) {
    if (body.tag !== "text" || typeof body.val !== "string") {
        throw new TypeError("text variant did not map to the expected tagged object");
    }
    return parseAndSerializeJson(body.val);
}

// This is the exact UTF-8 decoding helper documented to generated handlers.
// QuickJS does not provide TextDecoder, so bytes-only JSON handlers must pay
// this guest-side conversion cost before JSON.parse.
function decodeUtf8(bytes) {
    if (!(bytes instanceof Uint8Array)) {
        throw new TypeError("list<u8> did not map to Uint8Array");
    }

    // JSON and most protocol text are ASCII. Decode that common case in
    // bounded chunks, avoiding a percent-encoded temporary string per byte.
    let ascii = true;
    for (let i = 0; i < bytes.length; i++) {
        if (bytes[i] > 0x7f) {
            ascii = false;
            break;
        }
    }
    if (ascii) {
        let text = "";
        const chunkSize = 8192;
        for (let i = 0; i < bytes.length; i += chunkSize) {
            text += String.fromCharCode.apply(
                null,
                bytes.subarray(i, Math.min(i + chunkSize, bytes.length))
            );
        }
        return text;
    }

    // Strict UTF-8 fallback: malformed sequences throw URIError.
    let encoded = "";
    for (const byte of bytes) {
        encoded += "%" + byte.toString(16).padStart(2, "0");
    }
    return decodeURIComponent(encoded);
}

export function handleBytesOnly(body) {
    return parseAndSerializeJson(decodeUtf8(body));
}

export function serializeResponse(payload) {
    return JSON.stringify({ payload });
}

export function roundtripBinary(body) {
    if (!(body instanceof Uint8Array)) {
        throw new TypeError("binary list<u8> did not map to Uint8Array");
    }
    return body;
}
