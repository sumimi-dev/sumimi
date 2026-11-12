/**
 * Sumimi — the tracker, typed.
 *
 * The tracker itself is one file of plain JavaScript, served by the Sumimi
 * server itself at `GET /sumimi.js` — the file in the repository is
 * byte-for-byte the file that gets served. This package is the typed way to
 * use it from TypeScript:
 *
 * ```ts
 * // npm
 * import { init } from "@sumimi/tracker";
 * // or JSR
 * import { init } from "jsr:@ljh/sumimi";
 *
 * const sumimi = init({ website: "<website-uuid>" });
 * sumimi.track("signup", { plan: "pro" });
 * ```
 *
 * The script is injected once and loaded once; the calls made before it is
 * ready are queued and sent when it is.
 *
 * ## What it reports
 *
 * A page view, a screen view, or a custom event — with the same fields
 * Umami reports, and without cookies, without fingerprints and without storing
 * raw IP addresses.
 *
 * ## The privacy defaults
 *
 * Do Not Track and Global Privacy Control are honoured by the tracker itself:
 * if either says "no", nothing is sent at all.
 */

/** The options of the tracker. */
export interface SumimiOptions {
  /**
   * The website to report to — the id from the dashboard, or the domain.
   */
  website: string;
  /**
   * The base URL of the Sumimi API. Defaults to the origin of the page.
   */
  apiUrl?: string;
  /**
   * The URL of the tracker script. Defaults to `<apiUrl>/sumimi.js`.
   */
  scriptUrl?: string;
  /**
   * The name of the event to report. Custom events are what the dashboard's
   * "events" report counts.
   */
  // deno-lint-ignore no-empty-interface
}

/** The tracker, once it is running. */
export interface Sumimi {
  /**
   * Send a custom event.
   *
   * @param name The name of the event — `signup`, `download`, …
   * @param data Anything worth keeping with it.
   */
  track(name: string, data?: Record<string, unknown>): void;
  /**
   * Attach data to the current session.
   *
   * @param data Anything worth keeping with it.
   */
  identify(data: Record<string, unknown>): void;
  /**
   * Send a page view by hand.
   *
   * The tracker sends one automatically when the page loads, and another when
   * a single-page app changes the route. This is for the cases where neither
   * happens — a page that is not a navigation, for instance.
   *
   * @param url The path to report. Defaults to the current one.
   */
  pageview(url?: string): void;
}

/**
 * The bits of the DOM this package needs.
 *
 * They are declared here rather than taken from `lib.dom` — so the package
 * type-checks in Deno, in Node and in a browser build, without anybody having
 * to add a `lib` setting.
 */
interface ScriptElement {
  async: boolean;
  src: string;
  setAttribute(name: string, value: string): void;
  addEventListener(type: string, listener: () => void): void;
}

interface DocumentLike {
  createElement(tag: "script"): ScriptElement;
  head: { appendChild(node: ScriptElement): void };
}

/** A call that is waiting for the tracker to be ready. */
type Pending = (api: Sumimi) => void;

/** The tracker that is running, if there is one. */
let current: Sumimi | undefined;

/** The calls that were made before the tracker was ready. */
const pending: Pending[] = [];

/** The script that was injected, if it already is. */
let script: ScriptElement | undefined;

/** The API the tracker exposes once it has loaded. */
interface TrackerWindow {
  sumimi?: Sumimi;
  umami?: Sumimi;
}

/**
 * Start the tracker.
 *
 * The script is injected the first time this is called; calling it again
 * returns the same tracker. The calls made before the script has loaded are
 * queued, and sent when it is ready — nothing is lost.
 *
 * @param options Where to report to, and to which server.
 * @returns The tracker.
 */
export function init(options: SumimiOptions): Sumimi {
  if (current) {
    return current;
  }

  const api: Sumimi = {
    track(name: string, data?: Record<string, unknown>): void {
      send((tracker) => tracker.track(name, data));
    },
    identify(data: Record<string, unknown>): void {
      send((tracker) => tracker.identify(data));
    },
    pageview(url?: string): void {
      send((tracker) => tracker.pageview(url));
    },
  };

  current = api;

  const apiUrl = (options.apiUrl ?? origin()).replace(/\/$/, "");
  const src = options.scriptUrl ?? `${apiUrl}/sumimi.js`;

  if (!script) {
    const document = dom();
    script = document.createElement("script");
    script.async = true;
    script.src = src;
    script.setAttribute("data-website-id", options.website);
    script.setAttribute("data-api-url", apiUrl);
    script.addEventListener("load", flush);
    document.head.appendChild(script);
  } else {
    // The script is already there: it has its own attributes, so nothing to do
    // but hand over the calls that were waiting.
    flush();
  }

  return api;
}

/**
 * Send a custom event.
 *
 * The tracker has to be running: call {@link init} first.
 *
 * @param name The name of the event.
 * @param data Anything worth keeping with it.
 */
export function track(name: string, data?: Record<string, unknown>): void {
  require().track(name, data);
}

/**
 * Attach data to the current session.
 *
 * The tracker has to be running: call {@link init} first.
 *
 * @param data Anything worth keeping with it.
 */
export function identify(data: Record<string, unknown>): void {
  require().identify(data);
}

/**
 * Send a page view by hand.
 *
 * The tracker has to be running: call {@link init} first.
 *
 * @param url The path to report.
 */
export function pageview(url?: string): void {
  require().pageview(url);
}

/** The tracker, or a loud error. */
function require(): Sumimi {
  if (!current) {
    throw new Error(
      "the Sumimi tracker is not running yet — call init({ website }) first",
    );
  }
  return current;
}

/** Hand a call over to the tracker, or queue it until the tracker is ready. */
function send(call: Pending): void {
  const tracker = (globalThis as unknown as { window?: TrackerWindow }).window
    ?.sumimi;
  if (tracker) {
    call(tracker);
    return;
  }
  pending.push(call);
}

/** Send everything that was waiting. */
function flush(): void {
  const tracker = (globalThis as unknown as { window?: TrackerWindow }).window
    ?.sumimi;
  if (!tracker) {
    return;
  }
  while (pending.length > 0) {
    const call = pending.shift();
    if (call) {
      call(tracker);
    }
  }
}

/** The document, or a loud error where there is none. */
function dom(): DocumentLike {
  const document = (globalThis as unknown as { document?: DocumentLike }).document;
  if (!document) {
    throw new Error(
      "the Sumimi tracker needs a browser — there is no document here",
    );
  }
  return document;
}

/** The origin of the page, or an empty string where there is none. */
function origin(): string {
  const location = (globalThis as unknown as { location?: { origin?: string } })
    .location;
  return location?.origin ?? "";
}
