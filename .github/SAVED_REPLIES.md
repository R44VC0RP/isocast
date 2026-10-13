# Saved replies

Replies maintainers paste into issues and pull requests. They stay kind
and say exactly what to do next. To use them on GitHub, add them under
Settings → Saved replies, or copy them from here.

## Bigger change without an accepted issue

> Thanks for putting this together! Changes this size (features, new
> dependencies, refactors, UI changes beyond a fix) start with an issue,
> so we can agree on the approach before anyone spends time on code. I'm
> closing this for now. Please open an issue describing what it does and
> why, link this pull request, and once it has the `accepted` label, reopen
> this or open a new one. See CONTRIBUTING.md, "What to send, and where".

## UI change without screenshots

> Thanks! This changes the app's interface, so it needs before and after
> screenshots in the description. `scripts/ui-screenshots.sh before/` on
> main and `scripts/ui-screenshots.sh after/` on your branch make a
> consistent set in about a minute. Drag the relevant ones in, and the
> check re-runs when you edit the description.

## No evidence of verification

> Thanks! The "How it was verified" section needs the commands you (or
> your agent) ran and what they printed: the tests, the real `isocast`
> commands, `ffprobe` output for recordings, and `scripts/bench.py` numbers
> for performance claims. Please fill it in. The check re-runs when you edit
> the description.

## Unverified machine output

> Thanks for the interest. This looks like code that hasn't been run or
> checked: the description doesn't show it working, and parts of it don't
> build or don't match how Isocast works. Coding agents are welcome here.
> Use the latest state-of-the-art model at its highest reasoning setting,
> point it at AGENTS.md, and have it run and show the evidence. I'm closing
> this one; a verified version is welcome.

## Clean-room concern

> Thanks! Isocast is written clean-room: no OBS Studio source code,
> shaders, theme or UI files may be read or copied, because OBS is GPL and
> Isocast is MIT OR Apache-2.0. Parts of this look like they follow OBS's
> code closely. Could you confirm how it was written? If it came from OBS's
> source, we can't take it, but a version written from OBS's public docs or
> its observed behaviour is welcome.

## Question or idea, not a bug

> Thanks for asking! Questions and ideas are best in
> [Discussions](https://github.com/R44VC0RP/isocast/discussions), where
> others can find the answer too. I've moved this there.

## Accepted

> Thanks, this is a good fit. I've added the `accepted` label: go ahead
> with a pull request that links this issue. Remember the evidence (see
> CONTRIBUTING.md): screenshots for UI changes, `ffprobe` output for
> recording changes, and `bench.py` numbers for performance.

## Landed

> Thank you! This has been applied upstream with you as the author and is
> in the snapshot commit <link>. Closing this pull request since it landed
> there.

## Security report in public

> Thanks for reporting this. Security problems should be reported
> privately, so I've hidden the details here. Please send them through
> [Report a vulnerability](https://github.com/R44VC0RP/isocast/security/advisories/new)
> (see SECURITY.md), and we'll follow up there.
