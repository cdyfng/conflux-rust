region=us-west-1
aws ec2 stop-instances --force --region $region --instance-ids `cat instances`
#for region in us-east-2 us-west-1 ap-southeast-1 ap-southeast-2 eu-central-1 sa-east-1
#do
#    instances=`aws ec2 describe-instances --region $region --filters "Name=tag:Name,Values=saber*" --query 'Reservations[*].Instances[*].[InstanceId]'`
#    aws ec2 start-instances --region $region --instance-ids $instances
#done
