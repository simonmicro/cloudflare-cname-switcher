This is a Python script to update a (here called) dynamic CNAME on Cloudflare to either a primary CNAME or a secondary CNAME.
This is useful e.g. when you have one CNAME which is fast but sometimes cuts out and an other one, which is slow but stable and you want to use to route everything over
during any downtime of the primary.

Also the default configuration will tend to prefer the secondary (with higher TTLs and a confidence level for the primary).

_Please note:_ This script uses a ton of external ip providers and it can take up to 24 hours after starting until they are rated properly (some tend to be unavailable
or just report wrong public ip addresses) - during this period the script will switch without reason between primary and secondary!
Also this script supports only IPv4 for now.
