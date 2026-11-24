# Service: corpus-normalize-json

## Capabilities
- state: volatile
- egress: []

## Endpoints
### `POST /normalize`
- Rules: Trim the name and normalize it to lowercase.
- Response `200`: JSON object containing the normalized name.

## Verification
### `POST /normalize`
- Body: {"name":" ADA "}
- Response `200`
- Body contains: ada
