from ipgetter2 import IPGetter
from urllib.request import Request, urlopen
import os
import json
import time
import ipaddress
import configparser
import logging
logging.basicConfig(format='%(asctime)s - %(levelname)s - %(message)s', level=logging.INFO)
logger = logging.getLogger(__name__)

# Config stuff
config = configparser.ConfigParser(allow_no_value=True)
os.makedirs('config', exist_ok=True)
configPath = os.path.join('config', 'config.ini')
if os.path.exists(configPath) == False:
    config['Cloudflare'] = {
        '# Open the overview of the domain and look bottom-right...': None,
        'zone_id': '',
        '# Cloudflare account -> API-Token -> Create a new one with the Zone.DNS permission': None,
        'token': ''
    }
    config['General'] = {
        '# Enable this when something does not work...': None,
        'debug': 'false',
        '# This CNAME will by updated when the external ip leaves the primary subnet or enters the secondary subnet': None,
        'dynamic_cname': 'dyn.example.com',
        '# Please note the Client API are rate-limited by Cloudflare account to 1200 requests every 5 minutes': None,
        'update_interval': '30',
        '# We\'ll try to get the external ip from up to 3 servers, each with a time of x': None,
        'external_timeout': '10',
        '# You can here specify e.g. \'http://icanhazip.com/\' to enforce using only one specific resolver (in case the \'default\' are too unstable)...': None,
        'external_resolver': 'default'
    }
    config['Primary'] = {
        '# E.g. primary cable line': None,
        'CNAME': 'wan1.example.com',
        '# Commonly found by try-and-error - the following modes are supported:': None,
        '# - Only Primary.Subnet: Switch to primary when external IP enters it long enough.': None,
        '# - Only Secondary.Subnet: Switch to secondary when external IP enters it.': None,
        '# - Both subnets: Switch to primary when external IP enters it long enough and switch to secondary when external IP enters it. Otherwise do nothing.': None,
        'Subnet': '88.42.0.0/24',
        '# TTL to be applied to dynamic_cname when this is active': None,
        'TTL': '60',
        '# Amount of successful checks needed, until we switch back to primary from secondary': None,
        'Confidence': '4'
    }
    config['Secondary'] = {
        '# E.g. the fallback over mobile network': None,
        'CNAME': 'wan2.example.com',
        '# Commonly found by try-and-error (set to \'no\' to disable)': None,
        'Subnet': 'no',
        '# TTL to be applied to dynamic_cname when this is active (higher to prevent clients constantly switching when the network is bad)': None,
        'TTL': '300'
    }
    with open(configPath, 'w') as configfile:
        config.write(configfile)
        logger.info('Missing ' + configPath + ' -> written default one.')
        exit(0)
config.read(configPath)

if config.getboolean('General', 'debug'):
    logging.basicConfig(format='%(asctime)s - %(levelname)s - %(message)s', level=logging.DEBUG, force=True)

# Resolve the dynamic_cname to a dns entry id of Cloudflare
request = Request(
    'https://api.Cloudflare.com/client/v4/zones/' + config['Cloudflare']['zone_id'] + '/dns_records?name=' + config['General']['dynamic_cname'],
    method='GET',
    headers={
        'Authorization': 'Bearer ' + config['Cloudflare']['token'],
        'Content-Type': 'application/json'
        }
    )
CloudflareDnsRecordId = None
try:
    for dns in json.load(urlopen(request))['result']:
        if dns['name'] == config['General']['dynamic_cname']:
            CloudflareDnsRecordId = dns['id']
            break
except:
    pass
if CloudflareDnsRecordId is None:
    logger.critical('Could not resolve ' + config['General']['dynamic_cname'] + ' to a Cloudflare dns id!')
    exit(1)
logger.debug(config['General']['dynamic_cname'] + ' is ' + CloudflareDnsRecordId)

logger.info('Startup complete.')
getter = IPGetter()
getter.timeout = int(config['General']['external_timeout'])
primaryConfidence = int(int(config['Primary']['confidence']) / 2)
primaryActive = False
primarySubnetSet = config['Primary']['subnet'] != 'no'
secondarySubnetSet = config['Secondary']['subnet'] != 'no'
if primarySubnetSet:
    primarySubnet = ipaddress.ip_network(config['Primary']['subnet'])
if secondarySubnetSet:
    secondarySubnet = ipaddress.ip_network(config['Secondary']['subnet'])
bothSubnetSet = not (primarySubnetSet ^ secondarySubnetSet)
try:
    while True:
        # Get the external ip and validate primary cname allowance
        try:
            logger.debug('Resolving external IPv4...')
            if config['General']['external_resolver'] == 'default':
                externalIPv4 = ipaddress.ip_address(str(getter.get().v4))
            else:
                externalIPv4 = ipaddress.ip_address(str(getter.get_from(config['General']['external_resolver']).v4))
            externalIsPrimary = primarySubnetSet and externalIPv4 in primarySubnet
            externalIsSecondary = secondarySubnetSet and externalIPv4 in secondarySubnet
            if primarySubnetSet and externalIsPrimary or (secondarySubnetSet and not externalIsSecondary and not bothSubnetSet):
                primaryConfidence += 1
            elif secondarySubnetSet and externalIsSecondary or (primarySubnetSet and not externalIsPrimary and not bothSubnetSet):
                primaryConfidence = 0
            else:
                logger.warning('External IP (' + str(externalIPv4) + ') is in neither the primary (' + str(primarySubnet) + ') nor the secondary (' + str(secondarySubnet) + ') subnet -> ignoring...')
            logger.debug('External IP is ' + str(externalIPv4))
        except Exception as e:
            logger.warning('External IPv4 resolve error: ' + str(e))
            primaryConfidence = 0

        # And update the dns entry of Cloudflare...
        def updateDynamicCname(config, data):
            try:
                urlopen(Request(
                    'https://api.Cloudflare.com/client/v4/zones/' + config['Cloudflare']['zone_id'] + '/dns_records/' + CloudflareDnsRecordId,
                    method='PUT',
                    data=bytes(json.dumps(data), encoding='utf8'),
                    headers={
                        'Authorization': 'Bearer ' + config['Cloudflare']['token'],
                        'Content-Type': 'application/json'
                    }
                ))
                logger.info('Updated ' + config['General']['dynamic_cname'] + ' to ' + data['content'])
            except Exception as e:
                logger.warning('Cloudflare CNAME update error: ' + str(e))

        if primaryConfidence == int(config['Primary']['confidence']) and not primaryActive:
            data = {
                'type': 'CNAME',
                'name': config['General']['dynamic_cname'],
                'content': config['Primary']['cname'],
                'ttl': int(config['Primary']['ttl']),
                'proxied': False
            }
            updateDynamicCname(config, data)
            primaryActive = True
        elif primaryConfidence == 0 and primaryActive:
            data = {
                'type': 'CNAME',
                'name': config['General']['dynamic_cname'],
                'content': config['Secondary']['cname'],
                'ttl': int(config['Secondary']['ttl']),
                'proxied': False
            }
            updateDynamicCname(config, data)
            primaryActive = False
        logger.debug('primaryConfidence? ' + str(primaryConfidence))
        
        # Wait until next check...
        logger.debug('Sleeping...')
        time.sleep(int(config['General']['update_interval']))
except KeyboardInterrupt:
    pass
        
logger.info('Bye!')
