FROM alpine

RUN apk --no-cache add python3 py3-pip
RUN pip3 install ipgetter2 ipaddress

WORKDIR /workdir
COPY cname_switcher.py .

CMD python3 cname_switcher.py
