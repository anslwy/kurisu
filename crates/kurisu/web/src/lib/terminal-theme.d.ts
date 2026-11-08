export interface ITheme { [color: string]: string }
export function terminalTheme(read: (name: string) => string): ITheme;
