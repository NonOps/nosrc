# Service: PII & Secret Redactor

A deterministic, stateless service that strips Personally Identifiable Information (PII) and common API secrets from query text before downstream LLM processing.

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `GET /redact`
- Rules: Read the `text` query parameter (URL-decoded). Return `400` if `text` is missing or empty. Redact sensitive tokens in this order: first replace API keys starting with `sk-`, `ghp_`, or `AKIA` (followed by alphanumeric/underscore/hyphen characters) with `[REDACTED:KEY]`; second replace valid email addresses (`user@domain.tld` where domain has a dot and TLD) with `[REDACTED:EMAIL]`; third replace US SSNs (`XXX-XX-XXXX`) with `[REDACTED:SSN]`; fourth replace phone numbers like `+1-415-555-0199` or `(555) 234-5678` with `[REDACTED:PHONE]`; fifth replace IPv4 addresses with `[REDACTED:IP]`. Leave malformed strings without a full domain untouched.
- Response `200`: `{ "redacted_text": "Contact [REDACTED:EMAIL] for support", "counts": { "email": 1, "phone": 0, "ssn": 0, "api_key": 0, "ipv4": 0 } }`
- Response `400`: `{ "error": "missing text query parameter" }`

## Examples

A representative request and response:

- Request: `GET /redact?text=Email%20alice@example.com%20for%20details`
- Response body: `{"redacted_text":"Email [REDACTED:EMAIL] for details","counts":{"email":1,"phone":0,"ssn":0,"api_key":0,"ipv4":0}}`

These examples are illustrative; the operator-controlled verification cases below remain the acceptance policy.

## Verification
### `GET /redact?text=Contact%20alice@example.com%20for%20support`
- Response `200`
- Body contains: `[REDACTED:EMAIL]`

### `GET /redact?text=Secret%20token%20sk-live-9876543210abcdef`
- Response `200`
- Body contains: `[REDACTED:KEY]`

### `GET /redact?text=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@`
- Response `200`
- Body contains: `aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@`

### `GET /redact`
- Response `400`
