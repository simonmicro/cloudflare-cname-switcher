FROM alpine

# Setup/Libs
RUN apk --no-cache add python3 py3-pip
RUN pip3 install ipgetter2 ipaddress

# Setup/Script
WORKDIR /workdir
COPY cname_switcher.py .

# Command
CMD python3 -u cname_switcher.py
