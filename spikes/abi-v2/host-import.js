import host from "nosrc:abi-v2-spike/host";

export function roundtrip(input) {
    return host.greet(input);
}
