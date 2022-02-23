/** ***************************************************************************
 * This file show you an example of how to call a smart contract
 * 
 * Once you ran the command `yarn run-sc 
 **/

import { generate_event, print } from "massa-sc-std";

export function main(_args: string): string {
    print("hehehe")
    
    generate_event("hello world")
    return "0"
}
