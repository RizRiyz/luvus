/**
 * Coalesces background redraws to at most one per animation frame, and holds
 * them while a pointer is pressed on the page.
 *
 * A full redraw replaces the page's elements. If that happens between a press
 * and its release, the element that was pressed no longer exists and the
 * browser never delivers the click, so a busy session makes the dashboard
 * feel frozen. Held redraws run as soon as the pointer is released, or after
 * `maxHoldMs` if the release is never seen.
 */
export class RenderScheduler {
  #queued = false;
  #held = false;
  #frameScheduled = false;
  #holdTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private readonly render: () => void,
    private readonly nextFrame: (callback: () => void) => void = (callback) => { requestAnimationFrame(() => callback()); },
    private readonly maxHoldMs = 1_000,
  ) {}

  /** Ask for a background redraw. Several requests produce one redraw. */
  request(): void {
    this.#queued = true;
    this.#scheduleFrame();
  }

  /** A pointer went down on the page: keep its target in place. */
  hold(): void {
    this.#held = true;
    if (this.#holdTimer) clearTimeout(this.#holdTimer);
    this.#holdTimer = setTimeout(() => this.release(), this.maxHoldMs);
  }

  /** The pointer was released or cancelled: run any held redraw. */
  release(): void {
    if (this.#holdTimer) clearTimeout(this.#holdTimer);
    this.#holdTimer = undefined;
    if (!this.#held) return;
    this.#held = false;
    if (this.#queued) this.#scheduleFrame();
  }

  #scheduleFrame(): void {
    if (this.#frameScheduled || this.#held) return;
    this.#frameScheduled = true;
    this.nextFrame(() => {
      this.#frameScheduled = false;
      if (this.#held || !this.#queued) return;
      this.#queued = false;
      this.render();
    });
  }
}
