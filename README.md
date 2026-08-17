# gister

Stand-up prep for people who don't remember what they did yesterday.

`gister` crawls a directory full of git repos, collects your commits since
yesterday (or any date you give it), and optionally asks an LLM to turn them into a
short plain-text summary you can read out loud before your coffee kicks in.

## Install

```sh
cargo install --git ssh://git@github.com/timvancann/gister.git --locked
```

## Use

```sh
gister ~/repos -s        # summarize yesterday's commits
gister --days 3 -s       # been away for a long weekend
gister --pull ~/repos    # pull everything 
gister ~/repos --claude  # let claude code summarize
```

Run `gister --help` for the rest.

## The name

*Gister* (/ˈɣɪstər/) is shorthand for the Dutch *gisteren*, meaning
*yesterday*, as commonly heard in Limburg. Which is precisely the question
this tool answers: "what did you do gister?" That it also reads as
**gist-er**, a thing that produces gists, is the kind of bilingual pun you
only get to make once, so here we are.
