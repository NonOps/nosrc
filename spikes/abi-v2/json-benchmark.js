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

export function serializeResponse(payload) {
    return JSON.stringify({ payload });
}

export function roundtripBinary(body) {
    if (!(body instanceof Uint8Array)) {
        throw new TypeError("binary list<u8> did not map to Uint8Array");
    }
    return body;
}
