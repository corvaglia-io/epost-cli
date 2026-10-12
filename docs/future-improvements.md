# Possible improvements

These are ideas for a fork, not implemented features or planned releases:

- **Authenticated website downloads:** some items have accessible metadata but
  return HTTP 404 through the public content API. A browser-session fallback
  could help, with explicit opt-in, account verification and the same content
  validation as API downloads. The current CLI recommends manual download.
- **Destination migration:** move delivery history between Paperless instances
  or local stores after verifying the existing originals.
- **Failed-upload recovery:** explicitly resubmit confirmed failures after
  checking whether an earlier upload was accepted.
- **Proxy authentication:** support machine credentials for sign-in proxies
  such as Cloudflare Access.
- **Optional ePost archiving:** a separate, explicit operation after verified
  delivery. It would need a reviewed mutation contract and safeguards for
  registered mail. Current ePost operations remain read-only.
