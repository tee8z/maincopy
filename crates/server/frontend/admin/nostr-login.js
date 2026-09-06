(() => {
  "use strict";

  const button = document.getElementById("nostr-login");
  const feedback = document.getElementById("nostr-login-status");
  if (!button || !feedback) return;

  const maxJsonBytes = 8 * 1024;
  const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
  const messages = {
    signer: "Unlock a Nostr signer extension and allow the sign-in request, then try again.",
    rejected: "The sign-in proof was not accepted. Check the selected Nostr key and try again.",
    busy: "Sign-in is busy. Wait a moment and try again.",
    response: "Sign-in could not be confirmed. Open administration to check your session, or retry.",
  };

  class SignInError extends Error {
    constructor(code) {
      super(code);
      this.code = code;
    }
  }

  async function readJson(response) {
    if (
      response.headers.get("content-type")?.split(";")[0].trim().toLowerCase() !== "application/json" ||
      !response.body
    ) {
      throw new SignInError("response");
    }
    const reader = response.body.getReader();
    const chunks = [];
    let length = 0;
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        length += value.byteLength;
        if (length > maxJsonBytes) {
          await reader.cancel();
          throw new SignInError("response");
        }
        chunks.push(value);
      }
    } finally {
      reader.releaseLock();
    }
    const bytes = new Uint8Array(length);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  }

  async function post(url, payload) {
    const body = JSON.stringify(payload);
    if (new TextEncoder().encode(body).byteLength > maxJsonBytes) {
      throw new SignInError("response");
    }
    const response = await fetch(url, {
      method: "POST",
      mode: "same-origin",
      credentials: "same-origin",
      redirect: "error",
      cache: "no-store",
      headers: { "content-type": "application/json", accept: "application/json" },
      body,
      signal: AbortSignal.timeout(15_000),
    });
    if (response.status === 429) throw new SignInError("busy");
    if (response.status === 401 || response.status === 403) throw new SignInError("rejected");
    if (response.status !== 201) throw new SignInError("response");
    return readJson(response);
  }

  function endpoint(path) {
    const url = new URL(path, window.location.origin);
    if (url.origin !== window.location.origin || url.search || url.hash) {
      throw new SignInError("response");
    }
    return url.href;
  }

  async function signProof(draft) {
    try {
      const signed = await window.nostr.signEvent({ ...draft, tags: draft.tags.map((tag) => [...tag]) });
      if (
        !signed || signed.kind !== draft.kind || signed.created_at !== draft.created_at ||
        signed.content !== draft.content || !Array.isArray(signed.tags) ||
        signed.tags.length !== draft.tags.length ||
        !signed.tags.every((tag, index) => Array.isArray(tag) && tag.length === 2 &&
          tag[0] === draft.tags[index][0] && tag[1] === draft.tags[index][1])
      ) {
        throw new SignInError("signer");
      }
      for (const [field, size] of [["id", 64], ["pubkey", 64], ["sig", 128]]) {
        if (typeof signed[field] !== "string" || signed[field].length !== size || !/^[0-9a-f]+$/.test(signed[field])) {
          throw new SignInError("signer");
        }
      }
      return { ...draft, id: signed.id, pubkey: signed.pubkey, sig: signed.sig };
    } catch {
      throw new SignInError("signer");
    }
  }

  async function signIn() {
    if (button.disabled) return;
    button.disabled = true;
    try {
      if (typeof window.nostr?.signEvent !== "function") throw new SignInError("signer");
      const challengeUrl = endpoint(button.dataset.challengePath);
      const sessionUrl = endpoint(button.dataset.sessionPath);
      feedback.textContent = "Requesting a sign-in challenge…";
      const challenge = await post(challengeUrl, { provider: "nostr" });
      if (
        !challenge || challenge.provider !== "nostr" ||
        typeof challenge.challenge_id !== "string" || !uuid.test(challenge.challenge_id) ||
        typeof challenge.challenge !== "string" ||
        challenge.challenge.length === 0 || challenge.challenge.length > 256
      ) {
        throw new SignInError("response");
      }
      feedback.textContent = "Approve the sign-in request in your Nostr signer.";
      const event = await signProof({
        created_at: Math.floor(Date.now() / 1000),
        kind: 27235,
        tags: [["u", sessionUrl], ["method", "POST"], ["challenge", challenge.challenge]],
        content: "",
      });
      feedback.textContent = "Confirming sign-in…";
      const session = await post(sessionUrl, {
        provider: "nostr",
        challenge_id: challenge.challenge_id,
        challenge: challenge.challenge,
        event: JSON.stringify(event),
      });
      if (!session || session.provider !== "nostr" ||
        typeof session.session_id !== "string" || !uuid.test(session.session_id) ||
        typeof session.user_id !== "string" || !uuid.test(session.user_id)) {
        throw new SignInError("response");
      }
      window.location.assign("/admin");
    } catch (error) {
      feedback.textContent = error instanceof SignInError ? messages[error.code] : messages.response;
    } finally {
      button.disabled = false;
    }
  }

  button.addEventListener("click", signIn);
  button.disabled = false;
})();
