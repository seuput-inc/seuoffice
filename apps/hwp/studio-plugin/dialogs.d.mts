/** Type surface for studio-plugin/dialogs.mjs (plain JS, bundled by Vite). */

export declare const DIALOGS_MODULE: string
export declare function studioDialogs(): {
  confirm(message: string): Promise<boolean>
  alert(message: string): Promise<void>
  prompt(message: string, value?: string): Promise<string | null>
}
