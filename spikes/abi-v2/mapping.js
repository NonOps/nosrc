import host from "nosrc:abi-v2-mapping/host";

export function probe() {
    const bytes = new Uint8Array([0, 1, 127, 255]);
    const value = {
        request: {
            meta: {
                requestId: "req-7",
                attempt: 2,
            },
        },
        kind: {
            tag: "bytes",
            val: bytes,
        },
        note: null,
        correlationId: "trace-9",
        raw: bytes,
    };

    const hostResult = host.inspect(value);
    const hasTextEncoder = typeof TextEncoder === "function";
    const hasTextDecoder = typeof TextDecoder === "function";
    let textCodecRoundtrip = false;

    if (hasTextEncoder && hasTextDecoder) {
        const encoded = new TextEncoder().encode("nosrc-abi-v2");
        textCodecRoundtrip = new TextDecoder().decode(encoded) === "nosrc-abi-v2";
    }

    return [
        hostResult,
        `uint8array=${value.raw instanceof Uint8Array}`,
        `text-encoder=${hasTextEncoder}`,
        `text-decoder=${hasTextDecoder}`,
        `text-codec-roundtrip=${textCodecRoundtrip}`,
    ].join(";");
}

export function decodeRequest(body) {
    if (!(body instanceof Uint8Array)) {
        throw new TypeError("list<u8> was not mapped to Uint8Array");
    }
    return body.length;
}

export function encodeResponse(size) {
    const body = new Uint8Array(size);
    body.fill(0x5a);
    return body;
}
