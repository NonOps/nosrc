# PII & Secret Redactor

## Purpose
A deterministic, stateless HTTP endpoint that strips Personally Identifiable Information (PII) and common API secrets from raw text or JSON logs before downstream LLM processing.

## Interface
- **Endpoint:** `POST /redact`
- **Request Body:** JSON object `{ "text": string, "mask_char"?: string }`
  - `text` (required): Input string up to 64 KB.
  - `mask_char` (optional): Replacement token prefix (defaults to `"[REDACTED]"`).
- **Response (`200 OK`):** JSON object:
  ```json
  {
    "redacted_text": "...",
    "counts": {
      "email": 0,
      "phone": 0,
      "ssn": 0,
      "api_key": 0,
      "ipv4": 0
    }
  }
  ```
- **Error Responses:**
  - `400 Bad Request`: Invalid JSON, missing `text` field, or `text` is not a string.
  - `413 Payload Too Large`: Input exceeds 65,536 bytes.

## Redaction Rules
1. **Emails:** Replace standard email addresses (`user@domain.tld`) with `[REDACTED:EMAIL]`.
2. **US SSNs:** Replace `XXX-XX-XXXX` patterns (excluding invalid `000`/`666`/`9XX` prefixes) with `[REDACTED:SSN]`.
3. **Phone Numbers:** Replace North American and E.164 international phone numbers (`+1-555-010-9999`, `(555) 234-5678`) with `[REDACTED:PHONE]`.
4. **API Keys & Tokens:** Replace common bearer/secret prefixes (`sk-...`, `ghp_...`, `AKIA[0-9A-Z]{16}`) with `[REDACTED:KEY]`.
5. **IPv4 Addresses:** Replace dotted-quad IPv4 addresses (`0-255` octets) with `[REDACTED:IP]`.
6. **Safety:** Avoid backtracking regexes; execution must complete well within fuel limits even on adversarial repeated-character inputs.

## Verification
- **Case 1 (Mixed PII):**
  - `POST /redact` with `{"text": "Contact alice@example.com or +1-415-555-0199; key sk-live-9876543210abcdef"}`
  - Expect `200`, `redacted_text` equals `"Contact [REDACTED:EMAIL] or [REDACTED:PHONE]; key [REDACTED:KEY]"`, `counts.email == 1`, `counts.phone == 1`, `counts.api_key == 1`.
- **Case 2 (ReDoS / Fuel Resilience):**
  - `POST /redact` with `{"text": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa@"}`
  - Expect `200` within normal fuel budget without timeout or trap.
- **Case 3 (Malformed Input):**
  - `POST /redact` with `{"not_text": 123}`
  - Expect `400`.
