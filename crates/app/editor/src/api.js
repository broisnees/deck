// Transport to Deck. Every op is a POST of JSON to /api/<op> on the page's own
// origin; a non-2xx reply carries {"error": "..."}. Outside of deckcode:// (on
// Windows WebView2 serves it as http://deckcode.localhost/) the in-memory mock
// answers instead.

import { createMock } from "./mock.js";

export const params = new URLSearchParams(location.search);
export const isMock = location.protocol !== "deckcode:" && location.hostname !== "deckcode.localhost";
const mock = isMock ? createMock(params) : null;

export class ApiError extends Error {}

export async function api(op, args = {}) {
  if (mock) {
    try {
      return await mock(op, args);
    } catch (e) {
      throw new ApiError(e.message);
    }
  }
  let res;
  try {
    res = await fetch(`/api/${op}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(args || {}),
    });
  } catch (e) {
    throw new ApiError(`${op}: ${e.message || "request failed"}`);
  }
  let data = null;
  try {
    data = await res.json();
  } catch {}
  if (!res.ok) throw new ApiError((data && data.error) || `${op} failed (${res.status})`);
  return data || {};
}
