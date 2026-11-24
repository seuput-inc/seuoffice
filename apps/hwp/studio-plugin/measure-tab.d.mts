import type { HwpDocument } from '../../../vendor/rhwp-core/rhwp.js'
import type { AgentTabMeasure, AgentTabSpot } from './agent-plugin.mjs'

export declare const NESTED_CONTROLS_TRIED: 3
export declare function measureTab(doc: HwpDocument, spot: AgentTabSpot): AgentTabMeasure | null
