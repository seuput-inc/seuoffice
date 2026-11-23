import { it } from 'vitest'
import * as historyServer from '@seuoffice/collab/server'
import { examples, verifyHistory } from '../../../apps/web/tests/collab-history-cases'
import { render } from './render'

for (const example of examples) {
  it(
    `${example.name}: the browser fixture forks, compensates and rejects an overlapping edit`,
    () => verifyHistory(historyServer, example, render),
    30_000,
  )
}
