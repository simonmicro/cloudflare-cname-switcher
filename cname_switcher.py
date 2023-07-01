import threading
import json
import time
import ipaddress
import yaml
import datetime
import argparse
import logging
import sys
logging.basicConfig(format='%(asctime)s - %(levelname)s - %(message)s', level=logging.INFO)
logger = logging.getLogger(__name__)

from ipgetter2 import IPGetter
from urllib.request import Request, urlopen
from http.server import HTTPServer, BaseHTTPRequestHandler
from prometheus_client import Gauge, Info, Enum, generate_latest, CollectorRegistry

parser = argparse.ArgumentParser()
parser.add_argument('--config', '-c', type=str, default='config.yml', help='Path to the configuration file')
parser.add_argument('--debug', '-d', action='store_true', help='Something does not work? Debug mode!')
parser.add_argument('--port', '-p', type=int, default=80, help='Port for the internal healthcheck/metrics-endpoint')
parser.add_argument('--metrics_prefix', type=str, default='ccs', help='Prefix for all metrics provided by this exporter')
args = parser.parse_args()

if args.debug:
    logging.basicConfig(format='%(asctime)s - %(levelname)s - %(message)s', level=logging.DEBUG, force=True)

# Config stuff
logger.debug('Loading config...')
with open(args.config, 'r') as configFile:
    config = yaml.safe_load(configFile)

# Stuff, which should be set, when the user is not using the sample-config anymore...
assert config['cloudflare']['zone_id'], 'cloudflare.zone_id should be given'
assert config['cloudflare']['token'], 'cloudflare.token should be given'
assert config['general']['dynamic_cname'], 'general.dynamic_cname should be given'
assert config['primary']['cname'], 'primary.cname should be given'
assert config['secondary']['cname'], 'secondary.cname should be given'
assert len(config['primary']['subnets']) > 0 or len(config['secondary']['subnets']) > 0, 'primary or secondary subnets should be given'

# Load config-elements
primaryConfidence = int(config['primary']['confidence'] / 2)
primarySubnets = [ipaddress.ip_network(n) for n in config['primary']['subnets']]
secondarySubnets = [ipaddress.ip_network(n) for n in config['secondary']['subnets']]
primarySubnetsGiven = len(primarySubnets) > 0
secondarySubnetsGiven = len(secondarySubnets) > 0
telegramToken = config['telegram']['token']
telegramTarget = config['telegram']['target']
if telegramToken is not None:
    assert telegramTarget, 'telegram.target should be given'
if config['general']['force_ipv4_only']:
    import urllib3
    urllib3.util.connection.HAS_IPV6 = False

def resolveNameToRecordId(config, name):
    logger.debug(f'Resolving {name} to a record-id...')
    request = Request(
        'https://api.cloudflare.com/client/v4/zones/' + config['cloudflare']['zone_id'] + '/dns_records?name=' + name,
        method='GET',
        headers={
            'Authorization': 'Bearer ' + config['cloudflare']['token'],
            'Content-Type': 'application/json'
            }
    )
    for dns in json.load(urlopen(request, timeout=config['general']['timeout']))['result']:
        if dns['name'] == name:
            logger.debug(name + ' record-id is ' + dns['id'])
            return dns['id']
    raise KeyError(name) # record with that name not found

# Resolve the dynamic_cname to a dns entry id of Cloudflare
try:
    CloudflareDnsRecordId = resolveNameToRecordId(config, config['general']['dynamic_cname'])
except:
    logger.exception('Could not resolve ' + config['general']['dynamic_cname'] + ' to a Cloudflare dns id!')
    sys.exit(1)
CloudflareDynDnsRecordId = None
if config['dyndns']['dyndns_target']:
    try:
        CloudflareDynDnsRecordId = resolveNameToRecordId(config, config['dyndns']['dyndns_target'])
    except:
        logger.exception('Could not resolve ' + config['dyndns']['dyndns_target'] + ' to a Cloudflare dns id!')
        sys.exit(2)

# Prepare the healthcheck/metric endpoint
loopTime = config['general']['update_interval']
metricRegistry = CollectorRegistry()
metricHealthy = Gauge(args.metrics_prefix + '_healthy', 'Everything OK?', registry=metricRegistry)
metricDurations = Gauge(args.metrics_prefix + '_durations', 'How long did it take to update XY?', ['dimension'], registry=metricRegistry)
metricCnameTarget = Enum(args.metrics_prefix + '_cname_target', 'Which CNAME is currently active?', states=['primary', 'secondary', 'undefined'], registry=metricRegistry)
metricCnameTarget.state('undefined') # initially we don't have anything set
metricExternalIp = Info(args.metrics_prefix + '_external_ip', 'Most recent external IP', registry=metricRegistry)
class HealthcheckMetricEndpoint(BaseHTTPRequestHandler):
    lastLoop = None

    def do_GET(self):
        self.protocol_version = 'HTTP/1.0'
        okay = self.lastLoop is not None and datetime.datetime.now() - self.lastLoop < datetime.timedelta(seconds=loopTime * 2)
        metricHealthy.set(1 if okay else 0)
        if self.path.endswith('/healthz'):
            msg = ('OK' if okay else 'BAD').encode('utf8')
            self.send_response(200 if okay else 503)
            self.send_header('Content-type', 'text/plain')
            self.send_header('Content-length', len(msg))
            self.end_headers()
            self.wfile.write(msg)
        elif self.path.endswith('/metrics'):
            self.send_response(200)
            self.send_header('Content-type', 'text/plain')
            self.end_headers()
            self.wfile.write(generate_latest(metricRegistry))
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, format, *args):
        # Do not print the healthcheck requests to the console!
        return

healthcheckServer = HTTPServer(('0.0.0.0', args.port), HealthcheckMetricEndpoint)
healthcheckThread = threading.Thread(target=healthcheckServer.serve_forever)
healthcheckThread.daemon = True # Disconnect from main thread
healthcheckThread.start()

# Configure the ipgetter
getter = IPGetter()
getter.timeout = config['general']['timeout']

logger.info('Startup complete.')
oldExternalIPv4 = None
externalIPv4 = None
primaryActive = False
ignoreFirstNotification = True
notificationBuffer = [] # In case sending a notification failes, it will be stored here...
if telegramToken is not None:
    metricQueuedTelegramNotifications = Gauge(args.metrics_prefix + '_queued_telegram_notifications', 'How many Telegram notifications are queued?', registry=metricRegistry)
    metricQueuedTelegramNotifications.set_function(lambda: len(notificationBuffer))
try:
    def sendTelegramNotification(message, markdown):
        global ignoreFirstNotification, notificationBuffer, logger
        if telegramToken is None:
            return
        if ignoreFirstNotification:
            ignoreFirstNotification = False
            return
        try:
            req = Request('https://api.telegram.org/bot' + telegramToken + '/sendMessage', method='POST')
            req.add_header('Content-Type', 'application/json')
            data = { 'chat_id': telegramTarget }
            if markdown:
                data['parse_mode'] = 'MarkdownV2'
                data['text'] = message.replace('.', '\\.')
            else:
                data['text'] = message
            data = json.dumps(data)
            data = data.encode()
            with metricDurations.labels(dimension='send_telegram').time():
                urlopen(req, timeout=config['general']['timeout'], data=data)
            logger.info('Sent Telegram notification successfully: ' + message.replace('\n', ' '))
            if len(notificationBuffer):
                retryThese = notificationBuffer
                notificationBuffer = [] # Empty current buffer
                logger.info(f'Processing {len(retryThese)} delayed massages...')
                for params in retryThese:
                    msg, markdown, timestamp = params
                    if markdown:
                        msg += f'\n\n_This is a delayed message from `{timestamp.isoformat()}`._'
                    else:
                        msg += f'\n\nThis is a delayed message from {timestamp.isoformat()}.'
                    try:
                        sendTelegramNotification(msg, markdown) # This will re-queue the message on failure...
                    except:
                        pass # Well... No.
        except:
            notificationBuffer.append((message, markdown, datetime.datetime.now(datetime.timezone.utc)))
            logger.exception('Telegram notification error.')

    while True:
        # Get the external ip and validate primary cname allowance
        with metricDurations.labels(dimension='loop').time():
            try:
                logger.debug('Resolving external IPv4...')
                with metricDurations.labels(dimension='external_ip').time():
                    if config['general']['external_resolver'] == 'default':
                        externalIPv4 = ipaddress.ip_address(str(getter.get().v4))
                    else:
                        externalIPv4 = ipaddress.ip_address(str(getter.get_from(config['general']['external_resolver']).v4))
                
                if externalIPv4 == ipaddress.IPv4Address('0.0.0.0'):
                    raise ValueError('External IPv4 is empty (0.0.0.0). Something seems wrong...')
                metricExternalIp.info({'ip': str(externalIPv4)})

                # Update the cname to the external ip...
                if CloudflareDynDnsRecordId is not None and oldExternalIPv4 != externalIPv4:
                    try:
                        data = {
                            'type': 'A',
                            'name': config['dyndns']['dyndns_target'],
                            'content': str(externalIPv4),
                            'ttl': config['dyndns']['dyndns_ttl'],
                            'proxied': False
                        }
                        request = Request(
                            'https://api.cloudflare.com/client/v4/zones/' + config['cloudflare']['zone_id'] + '/dns_records/' + CloudflareDynDnsRecordId,
                            method='PUT',
                            data=bytes(json.dumps(data), encoding='utf8'),
                            headers={
                                'Authorization': 'Bearer ' + config['cloudflare']['token'],
                                'Content-Type': 'application/json'
                            }
                        )
                        with metricDurations.labels(dimension='dyndns').time():
                            urlopen(request, timeout=config['general']['timeout'])
                        logger.info('Updated ' + config['dyndns']['dyndns_target'] + ' to ' + data['content'])
                        oldExternalIPv4 = externalIPv4 # Will be retried if not successful
                    except Exception as e:
                        logger.exception('Cloudflare A-record update error.')
                        sendTelegramNotification(f'Something went wrong at the Cloudflare A-record updater: {e}', False)
                
                externalIsPrimary = True in [externalIPv4 in n for n in primarySubnets]
                externalIsSecondary = True in [externalIPv4 in n for n in secondarySubnets]
                logger.debug(f'IP-Owner? externalIsPrimary {externalIsPrimary}, externalIsSecondary {externalIsSecondary}')
                if externalIsPrimary or (not primarySubnetsGiven and not externalIsSecondary):
                    primaryConfidence += 1
                elif externalIsSecondary or (not secondarySubnetsGiven and not externalIsPrimary):
                    primaryConfidence = 0
                else:
                    logger.warning('External IP (' + str(externalIPv4) + ') is in neither the primary (' + str(primarySubnets) + ') nor the secondary (' + str(secondarySubnets) + ') subnet -> ignoring...')
                logger.debug('External IP is ' + str(externalIPv4))
            except Exception as e:
                logger.exception('External IPv4 resolve error.')
                primaryConfidence = 0
                sendTelegramNotification(f'Something went wrong at the external IPv4 resolver: {e}', False)

            # And update the dns entry of Cloudflare...
            def updateDynamicCname(config, data) -> bool:
                try:
                    request = Request(
                        'https://api.cloudflare.com/client/v4/zones/' + config['cloudflare']['zone_id'] + '/dns_records/' + CloudflareDnsRecordId,
                        method='PUT',
                        data=bytes(json.dumps(data), encoding='utf8'),
                        headers={
                            'Authorization': 'Bearer ' + config['cloudflare']['token'],
                            'Content-Type': 'application/json'
                        }
                    )
                    with metricDurations.labels(dimension='cname_update').time():
                        urlopen(request, timeout=config['general']['timeout'])
                    logger.info('Updated ' + config['general']['dynamic_cname'] + ' to ' + data['content'])
                    return True
                except Exception as e:
                    logger.exception('Cloudflare CNAME-record update error.')
                    sendTelegramNotification(f'Something went wrong at the Cloudflare CNAME updater: {e}', False)
                    return False

            if primaryConfidence >= config['primary']['confidence'] and not primaryActive:
                data = {
                    'type': 'CNAME',
                    'name': config['general']['dynamic_cname'],
                    'content': config['primary']['cname'],
                    'ttl': config['primary']['ttl'],
                    'proxied': False
                }
                if updateDynamicCname(config, data):
                    primaryActive = True
                    metricCnameTarget.state('primary')
                    sendTelegramNotification(f'Primary network connection *STABLE* since `{primaryConfidence}` checks. Failover INACTIVE. Current IPv4 is `{externalIPv4}`.', True)
                else:
                    # CNAME update failed -> undefined state
                    metricCnameTarget.state('undefined')
            elif primaryConfidence == 0 and primaryActive:
                data = {
                    'type': 'CNAME',
                    'name': config['general']['dynamic_cname'],
                    'content': config['secondary']['cname'],
                    'ttl': config['secondary']['ttl'],
                    'proxied': False
                }
                if updateDynamicCname(config, data):
                    primaryActive = False
                    metricCnameTarget.state('secondary')
                    sendTelegramNotification(f'Primary network connection *FAILED*. Failover ACTIVE. Recheck in `{loopTime}` seconds... Current IPv4 is `{externalIPv4}`.', True)
                else:
                    # CNAME update failed -> undefined state
                    metricCnameTarget.state('undefined')
            logger.debug('primaryConfidence? ' + str(primaryConfidence))
            
            HealthcheckMetricEndpoint.lastLoop = datetime.datetime.now()

        # Wait until next check...
        logger.debug('Sleeping...')
        time.sleep(loopTime)
except KeyboardInterrupt:
    pass
        
logger.info('Bye!')
healthcheckServer.shutdown() # stop the healthcheck server