/**
 * Coalesces background redraws to at most one per animation frame, and holds
 * them while any pointer is pressed on the page.
 *
 * A full redraw replaces the page's elements. If that happens between a press
 * and its release, the element that was pressed no longer exists and the
 * browser never delivers the click, so a busy session makes the dashboard
 * feel frozen. Held redraws run once every pressed pointer is released. The
 * page also reports releases it cannot see directly (window blur, a hidden
 * tab, a mouse moving with no button down); `maxHoldMs` is only a last resort
 * so a release that is never reported cannot stop the page from updating.
 */
export class RenderScheduler {
  #queued = false;
  #frameScheduled = false;
  readonly #pressed = new Set<number>();
  #holdTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private readonly render: () => void,
    private readonly nextFrame: (callback: () => void) => void = (callback) => { requestAnimationFrame(() => callback()); },
    private readonly maxHoldMs = 10_000,
  ) {}

  /** Whether a pointer is pressed, so a redraw now could lose its click. */
  get holding(): boolean {
    return this.#pressed.size > 0;
  }

  /** Ask for a redraw. Several requests produce one redraw. */
  request(): void {
    this.#queued = true;
    this.#scheduleFrame();
  }

  /** Pointer `id` went down on the page: keep its target in place. */
  hold(id: number): void {
    this.#pressed.add(id);
    if (this.#holdTimer) clearTimeout(this.#holdTimer);
    this.#holdTimer = setTimeout(() => this.releaseAll(), this.maxHoldMs);
  }

  /** Pointer `id` was released or cancelled. */
  release(id: number): void {
    if (!this.#pressed.delete(id)) return;
    if (!this.holding) this.#resume();
  }

  /** Every press ended, including ones whose release the page never saw. */
  releaseAll(): void {
    if (!this.holding) return;
    this.#pressed.clear();
    this.#resume();
  }

  #resume(): void {
    if (this.#holdTimer) clearTimeout(this.#holdTimer);
    this.#holdTimer = undefined;
    if (this.#queued) this.#scheduleFrame();
  }

  #scheduleFrame(): void {
    if (this.#frameScheduled || this.holding) return;
    this.#frameScheduled = true;
    this.nextFrame(() => {
      this.#frameScheduled = false;
      if (this.holding || !this.#queued) return;
      this.#queued = false;
      this.render();
    });
  }
}
