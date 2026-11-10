// claude-hibernate's Claude Code mod: keeps the prompt draft across a sleep.
//
// The wrapper loads it with `--plugin-dir` (for that process only) and names
// two files in the environment:
//   CLAUDE_HIBERNATE_DRAFT_OUT  where to report the prompt box as it stands
//   CLAUDE_HIBERNATE_DRAFT_IN   on a resume, the draft to put back
// Without them it does nothing.
import type { Register } from 'claude-code'

let out: string | undefined
let last = ''
let lastAt = 0
/** When the draft was put back after a resume; -1 when the box never took it. */
let restored = 0
let draft = ''
let tries = 0

/** Writes the prompt box to `out` on a change, and every few seconds so the
 *  wrapper can tell a report is newer than the last key it passed through.
 *  The wrapper takes the draft from the last report before it stops Claude
 *  (as Claude exits its box already reads empty). */
async function report($: any, force: boolean) {
  if (!out) return
  const box = await $.prompt.read()
  const now: number = await $.clock.now()
  if (!force && box.text === last && now - lastAt < 5000) return
  last = box.text
  lastAt = now
  await $.fs.write(out, JSON.stringify({ text: box.text, cursor: box.cursor, at: now, restored }))
}

/** The prompt box mounts after session.start: try until it takes the draft. */
async function restore($: any) {
  const filled = await $.prompt.fill({ text: draft, mode: 'replace' })
  if (filled.isFilled || ++tries >= 100) {
    restored = filled.isFilled ? await $.clock.now() : -1
    await report($, true)
    return
  }
  $.clock.after(100, () => void restore($))
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    const r = await next(e)
    if (!e.isInteractive) return r
    out = await $.env.get('CLAUDE_HIBERNATE_DRAFT_OUT')
    if (!out) return r
    const from = await $.env.get('CLAUDE_HIBERNATE_DRAFT_IN')
    if (from) {
      try {
        draft = JSON.parse(await $.fs.read(from)).text ?? ''
      } catch {
        draft = '' // already put back (the wrapper removes the file), or none
      }
      await $.env.set('CLAUDE_HIBERNATE_DRAFT_IN', undefined)
    }
    if (draft) $.clock.after(0, () => void restore($))
    $.clock.every(1000, () => void report($, false))
    await report($, true)
    return r
  })
}
